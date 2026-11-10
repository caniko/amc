#pragma once

#include <chrono>
#include <cmath>
#include <cstdint>
#include <filesystem>
#include <fstream>
#include <limits>
#include <optional>
#include <sstream>
#include <stdexcept>
#include <string>
#include <sys/statvfs.h>
#include <thread>
#include <vector>

namespace nix {

// Set before the daemon forks; request settings cannot enlarge these budgets.
struct DaemonBuildPolicy
{
    std::filesystem::path slot;
    std::filesystem::path parent;
    uint64_t availableMin;
    uint64_t buildMax;
    uint64_t daemonReserve;
    unsigned int waitSeconds;
    unsigned int pressureSeconds;
    unsigned int pressurePercent;
    unsigned int memoryPsiMax;
    unsigned int ioPsiMax;
    uint64_t storeFreeMin;
};

inline uint64_t buildPolicyNumber(const std::filesystem::path & path)
{
    std::ifstream input(path);
    std::string token;
    if (!(input >> token) || token.empty() || token.find_first_not_of("0123456789") != std::string::npos)
        throw std::runtime_error("unreadable numeric build policy input: " + path.string());
    return std::stoull(token);
}

inline uint64_t buildPolicyAvailable(std::istream & input)
{
    std::string line;
    while (std::getline(input, line)) {
        std::istringstream fields(line);
        std::string key, unit;
        uint64_t kib;
        if (fields >> key && key == "MemAvailable:") {
            if (!(fields >> kib >> unit) || unit != "kB" || kib > std::numeric_limits<uint64_t>::max() / 1024)
                break;
            return kib * 1024;
        }
    }
    throw std::runtime_error("missing or invalid MemAvailable for build admission");
}

inline double buildPolicyFullPsi(std::istream & input)
{
    std::string line;
    while (std::getline(input, line)) {
        std::istringstream fields(line);
        std::string kind, token;
        if (!(fields >> kind) || kind != "full")
            continue;
        while (fields >> token) {
            if (token.rfind("avg10=", 0) != 0)
                continue;
            size_t end;
            double value = std::stod(token.substr(6), &end);
            if (end != token.size() - 6 || !std::isfinite(value) || value < 0 || value > 100)
                break;
            return value;
        }
    }
    throw std::runtime_error("missing or invalid full PSI for build policy");
}

inline double buildPolicyFullPsi(const std::filesystem::path & path)
{
    std::ifstream input(path);
    return buildPolicyFullPsi(input);
}

// First failing admission term with its value, or empty when the build fits.
inline std::string buildPolicyBlocker(const DaemonBuildPolicy & policy, uint64_t available,
    uint64_t parentCurrent, uint64_t parentMax, double memoryPsi, double ioPsi, uint64_t builderCurrent = 0)
{
    // Subtract in stages to avoid overflow/underflow, including saturated parents.
    // Idle builder page cache remains inside its fixed cap and is already
    // included in parentCurrent; reserving the whole cap again double-counts it.
    uint64_t additional = policy.buildMax > builderCurrent ? policy.buildMax - builderCurrent : 0;
    if (available < policy.availableMin)
        return "MemAvailable " + std::to_string(available) + " < " + std::to_string(policy.availableMin);
    if (parentCurrent > parentMax)
        return "parent memory.current " + std::to_string(parentCurrent) + " > memory.max " + std::to_string(parentMax);
    if (additional > parentMax - parentCurrent)
        return "build cap " + std::to_string(additional) + " > parent headroom " + std::to_string(parentMax - parentCurrent);
    if (policy.daemonReserve > parentMax - parentCurrent - additional)
        return "daemon reserve " + std::to_string(policy.daemonReserve) + " > headroom after build "
            + std::to_string(parentMax - parentCurrent - additional);
    if (!(memoryPsi < policy.memoryPsiMax))
        return "memory PSI full " + std::to_string(memoryPsi) + " >= " + std::to_string(policy.memoryPsiMax);
    if (!(ioPsi < policy.ioPsiMax))
        return "IO PSI full " + std::to_string(ioPsi) + " >= " + std::to_string(policy.ioPsiMax);
    return {};
}

inline bool buildPolicyFits(const DaemonBuildPolicy & policy, uint64_t available,
    uint64_t parentCurrent, uint64_t parentMax, double memoryPsi, double ioPsi, uint64_t builderCurrent = 0)
{
    return buildPolicyBlocker(policy, available, parentCurrent, parentMax, memoryPsi, ioPsi, builderCurrent).empty();
}

inline bool buildPolicyPopulated(std::istream & input)
{
    std::string line;
    while (std::getline(input, line)) {
        std::istringstream fields(line);
        std::string key, value;
        if (!(fields >> key >> value) || key != "populated")
            continue;
        if (value == "0" || value == "1")
            return value == "1";
        break;
    }
    throw std::runtime_error("missing or invalid builder populated state");
}

// io_ticks (ms with I/O in flight) is field 10 of /sys/block/<dev>/stat.
inline uint64_t buildPolicyIoTicks(std::istream & input)
{
    uint64_t value = 0;
    for (int field = 1; field <= 10; ++field)
        if (!(input >> value))
            throw std::runtime_error("invalid block device stat");
    return value;
}

// Busy means at least half of the sampling window had I/O in flight.
inline bool buildPolicyDiskBusy(uint64_t ticksBefore, uint64_t ticksAfter, uint64_t windowMs)
{
    return windowMs > 0 && ticksAfter >= ticksBefore && (ticksAfter - ticksBefore) * 2 >= windowMs;
}

// Global IO PSI also counts stalls from cgroup throttling and swap that no
// disk reflects. Corroborate it with real device activity. Fail closed (busy)
// when no physical device can be sampled.
// ponytail: one 1 s sample inside the admission retry loop; keep a rolling
// per-device window if the blocking sleep ever matters.
inline bool buildPolicyAnyDiskBusy(const std::filesystem::path & sysBlock = "/sys/block",
    std::chrono::milliseconds window = std::chrono::milliseconds(1000))
{
    auto sample = [&](std::vector<std::pair<std::filesystem::path, uint64_t>> & out) {
        out.clear();
        for (const auto & entry : std::filesystem::directory_iterator(sysBlock)) {
            if (!std::filesystem::exists(entry.path() / "device")) // virtual: loop, zram, dm, md
                continue;
            std::ifstream stat(entry.path() / "stat");
            out.emplace_back(entry.path(), buildPolicyIoTicks(stat));
        }
    };
    try {
        std::vector<std::pair<std::filesystem::path, uint64_t>> before, after;
        sample(before);
        auto start = std::chrono::steady_clock::now();
        std::this_thread::sleep_for(window);
        sample(after);
        auto elapsed = std::chrono::duration_cast<std::chrono::milliseconds>(std::chrono::steady_clock::now() - start).count();
        if (before.empty() || before.size() != after.size())
            return true;
        for (size_t i = 0; i < before.size(); ++i)
            if (before[i].first != after[i].first || buildPolicyDiskBusy(before[i].second, after[i].second, elapsed))
                return true;
        return false;
    } catch (const std::exception &) {
        return true;
    }
}

inline double buildPolicyIoPsi(const DaemonBuildPolicy & policy)
{
    double psi = buildPolicyFullPsi(std::filesystem::path("/proc/pressure/io"));
    return psi >= policy.ioPsiMax && !buildPolicyAnyDiskBusy() ? 0 : psi;
}

inline bool buildPolicyReady(const DaemonBuildPolicy & policy, const std::filesystem::path & builders)
{
    // A request-worker crash must not admit a new build while the old sandbox
    // is still exiting. Nix's PID namespace/death signal owns that cleanup.
    std::ifstream events(builders / "cgroup.events");
    if (buildPolicyPopulated(events))
        return false;
    std::ifstream memory("/proc/meminfo");
    return buildPolicyFits(policy, buildPolicyAvailable(memory),
        buildPolicyNumber(policy.parent / "memory.current"),
        buildPolicyNumber(policy.parent / "memory.max"),
        buildPolicyFullPsi(std::filesystem::path("/proc/pressure/memory")),
        buildPolicyIoPsi(policy),
        buildPolicyNumber(builders / "memory.current"));
}

inline bool buildPolicyDiskReady(const DaemonBuildPolicy & policy, const std::filesystem::path & store)
{
    struct statvfs stats;
    if (statvfs(store.c_str(), &stats) != 0 || stats.f_frsize == 0)
        throw std::runtime_error("cannot determine store free space for build admission");
    return stats.f_bavail >= policy.storeFreeMin / stats.f_frsize
        + (policy.storeFreeMin % stats.f_frsize != 0);
}

struct BuildPressureWindow
{
    using Clock = std::chrono::steady_clock;
    std::optional<Clock::time_point> since;

    bool exceeded(Clock::time_point now, double fullPsi, const DaemonBuildPolicy & policy)
    {
        if (fullPsi < policy.pressurePercent) {
            since.reset();
            return false;
        }
        if (!since || now < *since)
            since = now;
        return now - *since >= std::chrono::seconds(policy.pressureSeconds);
    }
};

} // namespace nix
