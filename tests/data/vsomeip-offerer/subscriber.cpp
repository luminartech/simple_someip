// vsomeip subscriber for phase-20h's TX-direction conformance test.
//
// Reverse of `offerer.cpp`: registers as a *requester* of service
// 0x1234 instance 0x0001, sets up an availability handler, and
// prints a stable [subscriber] AVAILABLE / UNAVAILABLE marker
// whenever vsomeip's SD subsystem decides the service is on/off
// the wire. The Rust test (`tests/vsomeip_sd_compat.rs`) drives
// `Server::announcement_loop` and scrapes our docker logs for the
// AVAILABLE marker as the assertion.
//
// Same hardcoded service+instance as the offerer — change one,
// change the other (see tests/vsomeip_sd_compat.rs constants).

#include <vsomeip/vsomeip.hpp>

#include <atomic>
#include <chrono>
#include <csignal>
#include <iostream>
#include <thread>

namespace {

constexpr vsomeip::service_t  kServiceId  = 0x1234;
constexpr vsomeip::instance_t kInstanceId = 0x0001;

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
        std::cerr << "[subscriber] vsomeip::runtime::get() returned null" << std::endl;
        return 1;
    }

    auto app = runtime->create_application("subscriber");
    if (!app) {
        std::cerr << "[subscriber] runtime->create_application() returned null" << std::endl;
        return 1;
    }

    if (!app->init()) {
        std::cerr << "[subscriber] application->init() failed; "
                  << "check VSOMEIP_CONFIGURATION and JSON validity" << std::endl;
        return 1;
    }

    // The availability handler fires whenever the routing manager's
    // view of the service changes (offered <-> stopped). Print a
    // distinct prefix so the Rust test can grep with low noise.
    app->register_availability_handler(
        kServiceId, kInstanceId,
        [](vsomeip::service_t srv, vsomeip::instance_t inst, bool available) {
            std::cout << "[subscriber] "
                      << (available ? "AVAILABLE" : "UNAVAILABLE")
                      << " service=0x" << std::hex << srv
                      << " instance=0x" << inst
                      << std::dec << std::endl
                      << std::flush;
        });

    // Drive vsomeip on a worker thread, the same shape as the offerer.
    std::thread vsomeip_thread([&app]() { app->start(); });

    // Brief warmup so vsomeip's SD subsystem is fully initialized
    // before we issue the request. Without this the request can
    // race past the SD-init code path on slower hosts and miss the
    // first round of incoming offers.
    std::this_thread::sleep_for(std::chrono::milliseconds(200));

    std::cout << "[subscriber] requesting service 0x" << std::hex << kServiceId
              << " instance 0x" << kInstanceId << std::dec << std::endl;

    app->request_service(kServiceId, kInstanceId);

    while (!g_shutdown.load(std::memory_order_acquire)) {
        std::this_thread::sleep_for(std::chrono::milliseconds(500));
    }

    std::cout << "[subscriber] shutdown requested; stopping vsomeip" << std::endl;
    app->release_service(kServiceId, kInstanceId);
    app->stop();
    vsomeip_thread.join();
    return 0;
}
