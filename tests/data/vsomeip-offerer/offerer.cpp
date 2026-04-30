// Minimal vsomeip offerer for phase-20f conformance testing.
//
// Offers service 0x1234 instance 0x0001 via vsomeip's SD subsystem.
// vsomeip emits OfferService SD broadcasts on the configured
// multicast group/port (per offerer.json's "service-discovery"
// section) until the process exits. That's the broadcast our
// `tests/vsomeip_sd_compat.rs` test on the host listens for.
//
// Hardcoded service+instance to keep this trivial; if the test's
// constants change, change them here too. See
// tests/vsomeip_sd_compat.rs:SERVICE_ID / INSTANCE_ID.

#include <vsomeip/vsomeip.hpp>

#include <atomic>
#include <chrono>
#include <csignal>
#include <iostream>
#include <thread>

namespace {

constexpr vsomeip::service_t  kServiceId  = 0x1234;
constexpr vsomeip::instance_t kInstanceId = 0x0001;
// Major.Minor version vsomeip advertises in OfferService entries.
// Defaults; doesn't have to match anything specific test-side.
constexpr vsomeip::major_version_t kMajor = 1;
constexpr vsomeip::minor_version_t kMinor = 0;

std::atomic<bool> g_shutdown{false};

void on_signal(int /*signum*/) {
    g_shutdown.store(true, std::memory_order_release);
}

}  // namespace

int main() {
    std::signal(SIGINT, on_signal);
    std::signal(SIGTERM, on_signal);

    auto runtime = vsomeip::runtime::get();
    if (!runtime) {
        std::cerr << "[offerer] vsomeip::runtime::get() returned null" << std::endl;
        return 1;
    }

    // Application name matches "applications" / "routing" entries in
    // offerer.json (and the VSOMEIP_APPLICATION_NAME env var the
    // Dockerfile sets). vsomeip uses this to look up the routing
    // configuration.
    auto app = runtime->create_application("offerer");
    if (!app) {
        std::cerr << "[offerer] runtime->create_application() returned null" << std::endl;
        return 1;
    }

    // init() reads the JSON config (VSOMEIP_CONFIGURATION) and
    // registers the SD subsystem.
    if (!app->init()) {
        std::cerr << "[offerer] application->init() failed; "
                  << "check VSOMEIP_CONFIGURATION and JSON validity" << std::endl;
        return 1;
    }

    // Spawn vsomeip's main loop on a worker thread. start() blocks
    // for the lifetime of the application; we drive it from a thread
    // so this main loop can monitor the shutdown signal.
    std::thread vsomeip_thread([&app]() { app->start(); });

    // Wait for vsomeip to be ready, then advertise the service.
    // 200 ms is more than enough for vsomeip's startup on any
    // x86 host.
    std::this_thread::sleep_for(std::chrono::milliseconds(200));

    std::cout << "[offerer] offering service 0x" << std::hex << kServiceId
              << " instance 0x" << kInstanceId
              << " (major " << std::dec << static_cast<int>(kMajor)
              << ", minor " << kMinor << ")" << std::endl;

    app->offer_service(kServiceId, kInstanceId, kMajor, kMinor);

    // Spin until SIGINT/SIGTERM. vsomeip's SD subsystem emits
    // periodic OfferService broadcasts in the background; we just
    // need to keep the process alive.
    while (!g_shutdown.load(std::memory_order_acquire)) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
    }

    std::cout << "[offerer] shutdown requested; stopping vsomeip" << std::endl;
    app->stop_offer_service(kServiceId, kInstanceId, kMajor, kMinor);
    app->stop();
    vsomeip_thread.join();
    return 0;
}
