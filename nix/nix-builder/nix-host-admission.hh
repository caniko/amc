#pragma once

#include <algorithm>
#include <atomic>
#include <cerrno>
#include <chrono>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <filesystem>
#include <fstream>
#include <optional>
#include <poll.h>
#include <sstream>
#include <stdexcept>
#include <string>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>
#include <utility>
#include <nlohmann/json.hpp>

namespace nix {

// Descriptors are opened independently by every daemon build coroutine and
// held through sandbox cleanup. The shared memory pool is a separate resource.
inline bool hostBuildSlotReady(int descriptor, bool required)
{
    if (!required || ::flock(descriptor, LOCK_EX | LOCK_NB) == 0)
        return true;
    if (errno == EWOULDBLOCK)
        return false;
    throw std::runtime_error("cannot lock daemon-wide Nix build capacity");
}

// The native owner holds one independently opened descriptor through cleanup.
// File anchors stay present; unlinking them could create two concurrent owners.
inline int openHostBuildSlot(const std::filesystem::path & base, unsigned int slots)
{
    if (slots > 64)
        throw std::runtime_error("invalid daemon-wide Nix build capacity");
    if (!slots) {
        int descriptor = ::open(base.c_str(), O_RDWR | O_CLOEXEC | O_NOFOLLOW);
        if (descriptor < 0)
            throw std::runtime_error("cannot open legacy Nix build capacity");
        return descriptor;
    }
    for (unsigned int index = 0; index < slots; ++index) {
        auto path = base.string() + "." + std::to_string(index);
        int descriptor = ::open(path.c_str(), O_RDWR | O_CLOEXEC | O_NOFOLLOW | O_CREAT, 0600);
        if (descriptor < 0)
            throw std::runtime_error("cannot open daemon-wide Nix build capacity");
        try {
            if (hostBuildSlotReady(descriptor, true))
                return descriptor;
        } catch (...) { ::close(descriptor); throw; }
        ::close(descriptor);
    }
    errno = EWOULDBLOCK;
    return -1;
}

inline unsigned int hostBuildLiveSlots(const std::filesystem::path & builders)
{
    unsigned int occupied = 0;
    for (const auto & entry : std::filesystem::directory_iterator(builders)) {
        if (!entry.is_directory())
            continue;
        std::ifstream events(entry.path() / "cgroup.events");
        std::string name;
        uint64_t value;
        bool known = false;
        while (events >> name >> value) {
            if (name == "populated") {
                if (value > 1)
                    throw std::runtime_error("invalid builder cgroup population");
                occupied += value;
                known = true;
                break;
            }
        }
        // A concurrent cleanup may remove a directory between listing/read.
        if (!known && std::filesystem::exists(entry.path()))
            throw std::runtime_error("unknown builder cgroup population");
    }
    return occupied;
}

// Only the daemon's socket credentials nominate an origin. Capture its kernel
// start identity before forking; client-provided settings are not authority.
inline std::optional<nlohmann::json> pinHostBuildOrigin(int pid)
{
    std::ifstream input("/proc/" + std::to_string(pid) + "/stat");
    std::string line;
    if (!std::getline(input, line))
        return std::nullopt;
    auto end = line.rfind(") ");
    if (end == std::string::npos)
        return std::nullopt;
    std::istringstream fields(line.substr(end + 2));
    std::string value;
    for (unsigned int i = 0; i <= 19; ++i)
        if (!(fields >> value))
            return std::nullopt;
    if (value.empty() || value.find_first_not_of("0123456789") != std::string::npos)
        return std::nullopt;
    return nlohmann::json{{"pid", pid}, {"start_ticks", std::stoull(value)}};
}

inline std::optional<nlohmann::json> & hostBuildOrigin()
{
    static std::optional<nlohmann::json> origin;
    return origin;
}

inline bool hostBuildPoolGranted(const std::string & response)
{
    auto value = nlohmann::json::parse(response);
    if (value.at("version") != 1 || !value.at("error").is_null())
        throw std::runtime_error("host admission refused the execution-owner pool");
    return value.at("granted").get<bool>();
}

// One bounded nonblocking attempt. Waiting remains in Nix's coroutine
// scheduler; the root broker owns durable aggregate-pool reservations.
inline bool hostBuildPoolRequest(const std::string & path, const nlohmann::json & message)
{
    if (path.empty())
        return false;
    sockaddr_un address{};
    address.sun_family = AF_UNIX;
    if (path.size() >= sizeof(address.sun_path))
        throw std::runtime_error("host admission socket path is too long");
    std::memcpy(address.sun_path, path.c_str(), path.size() + 1);
    int socket = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC | SOCK_NONBLOCK, 0);
    if (socket < 0)
        throw std::runtime_error("cannot open host admission connection");
    struct Close { int fd; ~Close() { ::close(fd); } } close{socket};
    auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(500);
    auto ready = [&](short events) {
        auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(deadline - std::chrono::steady_clock::now()).count();
        if (remaining <= 0)
            return false;
        pollfd descriptor{socket, events, 0};
        int result = ::poll(&descriptor, 1, int(std::min(int64_t(250), remaining)));
        return result > 0 && (descriptor.revents & events);
    };
    if (::connect(socket, reinterpret_cast<sockaddr *>(&address), sizeof(address)) != 0) {
        if ((errno != EINPROGRESS && errno != EAGAIN) || !ready(POLLOUT))
            throw std::runtime_error("host admission broker is unavailable");
        int error = 0;
        socklen_t length = sizeof(error);
        if (::getsockopt(socket, SOL_SOCKET, SO_ERROR, &error, &length) != 0 || error)
            throw std::runtime_error("host admission connection failed");
    }
    ucred peer{};
    socklen_t length = sizeof(peer);
    if (::getsockopt(socket, SOL_SOCKET, SO_PEERCRED, &peer, &length) != 0 || peer.uid != 0)
        throw std::runtime_error("host admission broker is not root-owned");
    auto request = message.dump() + "\n";
    size_t sent = 0;
    while (sent < request.size()) {
        if (!ready(POLLOUT))
            throw std::runtime_error("host admission write deadline expired");
        auto count = ::send(socket, request.data() + sent, request.size() - sent, MSG_NOSIGNAL);
        if (count <= 0)
            throw std::runtime_error("host admission write failed");
        sent += count;
    }
    std::string response;
    while (response.size() < 16384) {
        if (!ready(POLLIN))
            throw std::runtime_error("host admission read deadline expired");
        char buffer[1024];
        auto count = ::recv(socket, buffer, sizeof(buffer), 0);
        if (count <= 0)
            throw std::runtime_error("host admission response is incomplete");
        response.append(buffer, count);
        if (response.find('\n') != std::string::npos) {
            return hostBuildPoolGranted(response.substr(0, response.find('\n')));
        }
    }
    throw std::runtime_error("host admission response exceeds size bound");
}

inline uint64_t & currentHostBuildOperation()
{
    static thread_local uint64_t operation = 0;
    return operation;
}

class HostBuildOperation
{
    std::string path;
    uint64_t operation;
    uint64_t previous;
public:
    explicit HostBuildOperation(std::string path) : path(std::move(path))
    {
        static std::atomic<uint64_t> next{0};
        operation = ++next;
        previous = std::exchange(currentHostBuildOperation(), operation);
    }
    HostBuildOperation(const HostBuildOperation &) = delete;
    HostBuildOperation & operator=(const HostBuildOperation &) = delete;
    ~HostBuildOperation()
    {
        currentHostBuildOperation() = previous;
        if (path.empty()) return;
        try {
            if (!hostBuildPoolRequest(path, {{"op", "release_pool"}, {"version", 1},
                    {"domain", "nix-builders"}, {"operation", operation}}))
                throw std::runtime_error("pool release was refused");
        } catch (...) {
            // A failed release retains backing. The next worker uses a new
            // serial, so a stale PID grant cannot authorize its new work.
            std::fputs("AMC pool release unavailable; native backing retained\n", stderr);
        }
    }
};

inline bool acquireHostBuildPool(const std::string & path, unsigned int waitSeconds, bool continuations = false)
{
    auto message = nlohmann::json{{"op", "acquire_pool"}, {"version", 1},
        {"domain", "nix-builders"}, {"wait_ms", uint64_t(waitSeconds) * 1000}};
    if (continuations) {
        message["operation"] = currentHostBuildOperation();
        if (hostBuildOrigin()) message["origin"] = *hostBuildOrigin();
    }
    return hostBuildPoolRequest(path, message);
}

} // namespace nix
