#include "nix-host-admission.hh"
#include <cassert>
#include <iostream>
#include <fcntl.h>
#include <cstdlib>
#include <vector>

int main()
{
    // Sandboxed checks do not have the developer scratch directory.
    const char * temporary = std::getenv("TMPDIR");
    std::string pattern = std::string(temporary ? temporary : "/tmp") + "/nomad-nix-slot-XXXXXX";
    std::vector<char> name(pattern.begin(), pattern.end());
    name.push_back('\0');
    int first = ::mkstemp(name.data());
    assert(first >= 0);
    int second = ::open(name.data(), O_RDWR | O_CLOEXEC);
    assert(second >= 0);
    assert(nix::hostBuildSlotReady(first, true));
    assert(!nix::hostBuildSlotReady(second, true));
    assert(nix::hostBuildSlotReady(second, false));
    ::close(first);
    assert(nix::hostBuildSlotReady(second, true));
    ::close(second);
    ::unlink(name.data());
    auto base = std::string(name.data()) + "-bounded";
    int one = nix::openHostBuildSlot(base, 2);
    int two = nix::openHostBuildSlot(base, 2);
    assert(one >= 0 && two >= 0);
    assert(nix::openHostBuildSlot(base, 2) == -1 && errno == EWOULDBLOCK);
    ::close(one);
    int replacement = nix::openHostBuildSlot(base, 2);
    assert(replacement >= 0);
    ::close(two);
    ::close(replacement);
    assert(std::filesystem::exists(base + ".0") && std::filesystem::exists(base + ".1"));
    ::unlink((base + ".0").c_str());
    ::unlink((base + ".1").c_str());
    auto builders = std::filesystem::path(base + "-builders");
    std::filesystem::create_directories(builders / "live");
    std::filesystem::create_directories(builders / "cleanup");
    std::ofstream(builders / "live/cgroup.events") << "populated 1\nfrozen 0\n";
    std::ofstream(builders / "cleanup/cgroup.events") << "populated 0\nfrozen 0\n";
    assert(nix::hostBuildLiveSlots(builders) == 1);
    // A live sandbox still occupies capacity if its handler descriptor is gone.
    std::ofstream(builders / "cleanup/cgroup.events") << "populated 1\n";
    assert(nix::hostBuildLiveSlots(builders) == 2);
    std::ofstream(builders / "cleanup/cgroup.events") << "frozen 0\n";
    bool unknownPopulation = false;
    try { nix::hostBuildLiveSlots(builders); }
    catch (const std::exception &) { unknownPopulation = true; }
    assert(unknownPopulation);
    std::filesystem::remove_all(builders);
    auto origin = nix::pinHostBuildOrigin(getpid());
    assert(origin && origin->at("pid") == getpid());
    assert(origin->at("start_ticks").get<uint64_t>() > 0);
    assert(nix::pinHostBuildOrigin(getpid()) == origin);
    assert(!nix::pinHostBuildOrigin(-1));
    assert(!nix::hostBuildOrigin());
    assert(nix::currentHostBuildOperation() == 0);
    uint64_t outerSerial, innerSerial;
    {
        nix::HostBuildOperation outer("");
        outerSerial = nix::currentHostBuildOperation();
        assert(outerSerial > 0);
        {
            nix::HostBuildOperation inner("");
            innerSerial = nix::currentHostBuildOperation();
            assert(innerSerial > outerSerial);
        }
        assert(nix::currentHostBuildOperation() == outerSerial);
    }
    assert(nix::currentHostBuildOperation() == 0);
    {
        nix::HostBuildOperation newer("");
        assert(nix::currentHostBuildOperation() > innerSerial);
    }
    assert(nix::hostBuildPoolGranted(R"({"version":1,"error":null,"granted":true})"));
    assert(!nix::hostBuildPoolGranted(R"({"version":1,"error":null,"granted":false})"));
    for (auto response : {R"({"version":2,"error":null,"granted":true})",
             R"({"version":1,"error":"unknown pool","granted":true})",
             R"({"version":1,"error":null,"granted":"true"})", "{}", "broken"}) {
        bool rejected = false;
        try { nix::hostBuildPoolGranted(response); }
        catch (const std::exception &) { rejected = true; }
        assert(rejected);
    }
    assert(!nix::acquireHostBuildPool("", 300));
    bool rejected = false;
    try { nix::acquireHostBuildPool(std::string(4096, 'x'), 300); }
    catch (const std::exception &) { rejected = true; }
    assert(rejected);
    std::cout << "host admission, bounded capacity, cgroup population and nested worker lifetime PASS\n";
}
