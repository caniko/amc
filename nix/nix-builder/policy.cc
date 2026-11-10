#include "nix-build-policy.hh"

#include <cassert>
#include <fcntl.h>
#include <sys/file.h>
#include <sys/wait.h>
#include <unistd.h>

int main(int argc, char ** argv)
{
    assert(argc == 2);
    constexpr uint64_t gib = 1024ULL * 1024 * 1024;
    nix::DaemonBuildPolicy policy{"", "", 24 * gib, 16 * gib, gib / 2, 300, 30, 10, 1, 20, 20 * gib};
    std::istringstream memory("MemAvailable: 12582912 kB\nSwapFree: 67108864 kB\n");
    assert(nix::buildPolicyAvailable(memory) == 12 * gib);
    assert(!nix::buildPolicyFits(policy, 12 * gib, 0, 24 * gib, 0, 0));
    assert(nix::buildPolicyFits(policy, 24 * gib, 7 * gib, 24 * gib, 0, 0));
    assert(!nix::buildPolicyFits(policy, 24 * gib, 8 * gib, 24 * gib, 0, 0));
    assert(!nix::buildPolicyFits(policy, 24 * gib, 25 * gib, 24 * gib, 0, 0));
    assert(!nix::buildPolicyFits(policy, 24 * gib, 0, 24 * gib, 1, 0));
    assert(!nix::buildPolicyFits(policy, 24 * gib, 0, 24 * gib, 0, 20));
    // Idle page cache is already charged to both the builder and parent
    // domains, so only the unused portion of the builder cap needs admission.
    assert(nix::buildPolicyFits(policy, 24 * gib, 20 * gib, 24 * gib, 0, 0, 16 * gib));
    assert(!nix::buildPolicyFits(policy, 24 * gib, 20 * gib, 24 * gib, 0, 0, 8 * gib));
    assert(!nix::buildPolicyFits(policy, 24 * gib, 20 * gib, 24 * gib, 0, 0, 12 * gib));
    for (const auto & text : {"", "full avg10=NaN", "full avg10=10oops", "full avg10=-1", "full avg10=101"}) {
        std::istringstream input(text);
        bool failed = false;
        try { nix::buildPolicyFullPsi(input); } catch (const std::exception &) { failed = true; }
        assert(failed);
    }
    std::istringstream psi("some avg10=30\nfull avg10=10.25 avg60=8 total=100\n");
    assert(nix::buildPolicyFullPsi(psi) == 10.25);
    assert(nix::buildPolicyBlocker(policy, 24 * gib, 0, 24 * gib, 0, 20).rfind("IO PSI full 20", 0) == 0);
    assert(nix::buildPolicyBlocker(policy, 12 * gib, 0, 24 * gib, 0, 0).rfind("MemAvailable ", 0) == 0);
    assert(nix::buildPolicyBlocker(policy, 24 * gib, 0, 24 * gib, 0, 0).empty());
    // io_ticks is field 10; busy needs at least 50% of the window.
    std::istringstream diskStat("1 2 3 4 5 6 7 8 9 750 11 12");
    assert(nix::buildPolicyIoTicks(diskStat) == 750);
    std::istringstream shortStat("1 2 3");
    bool shortFailed = false;
    try { nix::buildPolicyIoTicks(shortStat); } catch (const std::exception &) { shortFailed = true; }
    assert(shortFailed);
    assert(nix::buildPolicyDiskBusy(1000, 1500, 1000));
    assert(!nix::buildPolicyDiskBusy(1000, 1499, 1000));
    assert(!nix::buildPolicyDiskBusy(1500, 1000, 1000));
    {
        // Physical device idle, virtual device ignored: PSI alone must not block.
        auto sys = std::filesystem::temp_directory_path() / ("sysblock-" + std::to_string(getpid()));
        std::filesystem::create_directories(sys / "nvme0n1" / "device");
        std::filesystem::create_directories(sys / "zram0");
        std::ofstream(sys / "nvme0n1" / "stat") << "1 2 3 4 5 6 7 8 9 750 11 12";
        std::ofstream(sys / "zram0" / "stat") << "1 2 3 4 5 6 7 8 9 750 11 12";
        assert(!nix::buildPolicyAnyDiskBusy(sys, std::chrono::milliseconds(50)));
        std::filesystem::remove_all(sys);
        assert(nix::buildPolicyAnyDiskBusy(sys, std::chrono::milliseconds(50))); // unreadable fails closed
    }
    std::istringstream emptyGroup("populated 0\nfrozen 0\n");
    std::istringstream occupiedGroup("populated 1\nfrozen 0\n");
    assert(!nix::buildPolicyPopulated(emptyGroup));
    assert(nix::buildPolicyPopulated(occupiedGroup));
    for (const auto & text : {"", "frozen 0", "populated 2", "populated -1"}) {
        std::istringstream input(text);
        bool failed = false;
        try { nix::buildPolicyPopulated(input); } catch (const std::exception &) { failed = true; }
        assert(failed);
    }
    policy.storeFreeMin = std::numeric_limits<uint64_t>::max();
    assert(!nix::buildPolicyDiskReady(policy, std::filesystem::path(argv[1]).parent_path()));
    policy.storeFreeMin = 0;
    assert(nix::buildPolicyDiskReady(policy, std::filesystem::path(argv[1]).parent_path()));
    nix::BuildPressureWindow window;
    const auto t = nix::BuildPressureWindow::Clock::time_point{};
    assert(!window.exceeded(t, 10, policy));
    assert(!window.exceeded(t + std::chrono::seconds(29), 10, policy));
    assert(window.exceeded(t + std::chrono::seconds(30), 10, policy));
    assert(!window.exceeded(t + std::chrono::seconds(31), 9.99, policy));
    assert(!window.exceeded(t + std::chrono::seconds(32), 10, policy));
    assert(!window.exceeded(t + std::chrono::seconds(61), 10, policy));
    assert(window.exceeded(t + std::chrono::seconds(62), 10, policy));

    // Independent opens contend even for two goals in the same handler.
    int fd = open(argv[1], O_RDWR | O_CREAT | O_CLOEXEC, 0600);
    assert(fd >= 0 && flock(fd, LOCK_EX | LOCK_NB) == 0);
    int sameProcess = open(argv[1], O_RDWR | O_CLOEXEC);
    assert(sameProcess >= 0 && flock(sameProcess, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK);
    close(sameProcess);
    // Exercise independent daemon request handlers too.
    pid_t child = fork();
    assert(child >= 0);
    if (child == 0) {
        close(fd);
        int contender = open(argv[1], O_RDWR | O_CLOEXEC);
        assert(contender >= 0 && flock(contender, LOCK_EX | LOCK_NB) == -1 && errno == EWOULDBLOCK);
        close(contender);
        _exit(0);
    }
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    close(fd);
    int next = open(argv[1], O_RDWR | O_CLOEXEC);
    assert(next >= 0 && flock(next, LOCK_EX | LOCK_NB) == 0);
    close(next);
}
