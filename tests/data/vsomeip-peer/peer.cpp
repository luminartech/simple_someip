// A vsomeip application driven by line commands on stdin. It reports what it
// observes, one line per event, on stdout, for simple-someip's interop tests.
#include <vsomeip/vsomeip.hpp>

#include <cstdio>
#include <iostream>
#include <map>
#include <mutex>
#include <set>
#include <stdexcept>
#include <sstream>
#include <string>
#include <thread>
#include <unistd.h>
#include <utility>
#include <vector>

namespace {

std::shared_ptr<vsomeip::application> app;
std::mutex out_mutex;
// Written by the stdin reader, read by vsomeip's dispatcher threads.
std::mutex error_replies_mutex;
std::map<vsomeip::method_t, vsomeip::return_code_e> error_replies;
// The major version each service was offered with; stop_offer_service must match it.
std::map<std::pair<uint16_t, uint16_t>, vsomeip::major_version_t> offered_majors;
// The line protocol's stream. vsomeip's console logger writes to std::cout,
// so main() moves the original stdout here and points fd 1 at stderr.
FILE *protocol_out = stdout;

void emit(const std::string &line) {
    std::lock_guard<std::mutex> lock(out_mutex);
    std::fputs(line.c_str(), protocol_out);
    std::fputc('\n', protocol_out);
    std::fflush(protocol_out);  // the test reads line by line
}

std::string hex16(uint32_t v) {
    char b[16];
    std::snprintf(b, sizeof b, "0x%04x", v);
    return b;
}

std::string hex8(uint32_t v) {
    char b[8];
    std::snprintf(b, sizeof b, "0x%02x", v);
    return b;
}

std::string to_hex(const vsomeip::byte_t *d, vsomeip::length_t n) {
    if (n == 0) return "-";
    std::string s;
    char b[3];
    for (vsomeip::length_t i = 0; i < n; ++i) {
        std::snprintf(b, sizeof b, "%02x", d[i]);
        s += b;
    }
    return s;
}

bool is_digit_in(char c, int base) {
    if (c >= '0' && c <= '9') return c - '0' < base;
    if (base != 16) return false;
    return (c >= 'a' && c <= 'f') || (c >= 'A' && c <= 'F');
}

// Parses all of `s` as an unsigned number in `base`, at most `max`. Throws
// std::invalid_argument otherwise, so the command fails with an ERR line.
uint32_t parse_num(const std::string &s, int base, uint32_t max) {
    if (s.empty()) throw std::invalid_argument("empty number");
    for (char c : s)
        if (!is_digit_in(c, base))
            throw std::invalid_argument("not a base-" + std::to_string(base) + " number: " + s);
    unsigned long v = 0;
    try {
        v = std::stoul(s, nullptr, base);
    } catch (const std::out_of_range &) {
        throw std::invalid_argument("out of range: " + s);
    }
    if (v > max) throw std::invalid_argument("out of range: " + s);
    return static_cast<uint32_t>(v);
}

std::vector<vsomeip::byte_t> from_hex(const std::string &s) {
    std::vector<vsomeip::byte_t> v;
    if (s == "-") return v;
    if (s.size() % 2 != 0) throw std::invalid_argument("odd-length hex payload: " + s);
    for (size_t i = 0; i < s.size(); i += 2)
        v.push_back(static_cast<vsomeip::byte_t>(parse_num(s.substr(i, 2), 16, 0xff)));
    return v;
}

uint16_t id(const std::string &s) { return static_cast<uint16_t>(parse_num(s, 16, 0xffff)); }

vsomeip::major_version_t major(const std::string &s) {
    return static_cast<vsomeip::major_version_t>(parse_num(s, 10, 0xff));
}

std::vector<uint16_t> ids(const std::string &s) {
    std::vector<uint16_t> v;
    if (s == "-") return v;
    std::stringstream ss(s);
    std::string item;
    while (std::getline(ss, item, ',')) v.push_back(id(item));
    return v;
}

std::shared_ptr<vsomeip::payload> make_payload(const std::string &hex) {
    auto p = vsomeip::runtime::get()->create_payload();
    auto bytes = from_hex(hex);
    p->set_data(bytes);
    return p;
}

void on_message(const std::shared_ptr<vsomeip::message> &m) {
    auto pl = m->get_payload();
    std::string payload = to_hex(pl->get_data(), pl->get_length());
    switch (m->get_message_type()) {
    case vsomeip::message_type_e::MT_NOTIFICATION:
        emit("EVENT service=" + hex16(m->get_service()) + " event=" + hex16(m->get_method()) +
             " payload=" + payload);
        break;
    case vsomeip::message_type_e::MT_REQUEST:
    case vsomeip::message_type_e::MT_REQUEST_NO_RETURN: {
        bool no_return = m->get_message_type() == vsomeip::message_type_e::MT_REQUEST_NO_RETURN;
        emit("REQUEST service=" + hex16(m->get_service()) + " method=" + hex16(m->get_method()) +
             " type=" + (no_return ? "request_no_return" : "request") +
             " session=" + hex16(m->get_session()) + " payload=" + payload);
        if (!no_return) {
            auto resp = vsomeip::runtime::get()->create_response(m);
            bool is_error = false;
            vsomeip::return_code_e code = vsomeip::return_code_e::E_OK;
            {
                std::lock_guard<std::mutex> lock(error_replies_mutex);
                auto it = error_replies.find(m->get_method());
                if (it != error_replies.end()) {
                    is_error = true;
                    code = it->second;
                }
            }
            if (is_error) {
                resp->set_message_type(vsomeip::message_type_e::MT_ERROR);
                resp->set_return_code(code);
            } else {
                resp->set_payload(pl);  // echo
            }
            app->send(resp);
        }
        break;
    }
    case vsomeip::message_type_e::MT_RESPONSE:
    case vsomeip::message_type_e::MT_ERROR:
        emit("RESPONSE service=" + hex16(m->get_service()) + " method=" + hex16(m->get_method()) +
             " type=" +
             (m->get_message_type() == vsomeip::message_type_e::MT_ERROR ? "error" : "response") +
             " return_code=" + hex8(static_cast<uint32_t>(m->get_return_code())) +
             " payload=" + payload);
        break;
    default:
        break;
    }
}

void offer_events(uint16_t svc, uint16_t inst, uint16_t eg, const std::vector<uint16_t> &evs,
                  vsomeip::event_type_e type) {
    for (auto e : evs)
        app->offer_event(svc, inst, e, {eg}, type, std::chrono::milliseconds::zero(), false, true,
                         nullptr, vsomeip::reliability_type_e::RT_UNRELIABLE);
}

void request_events(uint16_t svc, uint16_t inst, uint16_t eg, const std::vector<uint16_t> &evs,
                    vsomeip::event_type_e type) {
    for (auto e : evs)
        app->request_event(svc, inst, e, {eg}, type, vsomeip::reliability_type_e::RT_UNRELIABLE);
}

// Returns false on `quit`.
bool handle(const std::string &line) {
    std::stringstream ss(line);
    std::string cmd;
    ss >> cmd;
    std::vector<std::string> a;
    for (std::string t; ss >> t;) a.push_back(t);
    try {
        if (cmd == "offer" && a.size() == 7) {
            // Parse every argument before acting, so a rejected command changes nothing.
            uint16_t svc = id(a[0]), inst = id(a[1]), eg = id(a[3]);
            auto offered_major = major(a[2]);
            auto events = ids(a[4]), fields = ids(a[5]);
            ids(a[6]);  // methods: validated only; the handler accepts any method
            offer_events(svc, inst, eg, events, vsomeip::event_type_e::ET_EVENT);
            offer_events(svc, inst, eg, fields, vsomeip::event_type_e::ET_FIELD);
            app->register_message_handler(svc, inst, vsomeip::ANY_METHOD, on_message);
            app->offer_service(svc, inst, offered_major, 0);
            offered_majors[{svc, inst}] = offered_major;
        } else if (cmd == "stop-offer" && a.size() == 2) {
            uint16_t svc = id(a[0]), inst = id(a[1]);
            auto it = offered_majors.find({svc, inst});
            if (it == offered_majors.end()) {
                emit("ERR not offered: " + line);
                return true;
            }
            app->stop_offer_service(svc, inst, it->second, 0);
            offered_majors.erase(it);
        } else if ((cmd == "notify" || cmd == "set-field") && a.size() == 4) {
            app->notify(id(a[0]), id(a[1]), id(a[2]), make_payload(a[3]), true);
        } else if (cmd == "require" && a.size() == 3) {
            uint16_t svc = id(a[0]), inst = id(a[1]);
            auto required_major = major(a[2]);
            app->register_message_handler(svc, inst, vsomeip::ANY_METHOD, on_message);
            app->request_service(svc, inst, required_major, vsomeip::ANY_MINOR);
        } else if (cmd == "subscribe" && a.size() == 6) {
            uint16_t svc = id(a[0]), inst = id(a[1]), eg = id(a[2]);
            auto subscribed_major = major(a[3]);
            auto events = ids(a[4]), fields = ids(a[5]);
            request_events(svc, inst, eg, events, vsomeip::event_type_e::ET_EVENT);
            request_events(svc, inst, eg, fields, vsomeip::event_type_e::ET_FIELD);
            app->subscribe(svc, inst, eg, subscribed_major);
        } else if (cmd == "unsubscribe" && a.size() == 3) {
            app->unsubscribe(id(a[0]), id(a[1]), id(a[2]));
        } else if ((cmd == "call" || cmd == "fire") && a.size() == 4) {
            auto m = vsomeip::runtime::get()->create_request(false);
            m->set_service(id(a[0]));
            m->set_instance(id(a[1]));
            m->set_method(id(a[2]));
            if (cmd == "fire") m->set_message_type(vsomeip::message_type_e::MT_REQUEST_NO_RETURN);
            m->set_payload(make_payload(a[3]));
            app->send(m);
        } else if (cmd == "error-reply" && a.size() == 2) {
            auto method = id(a[0]);
            auto code = static_cast<vsomeip::return_code_e>(parse_num(a[1], 16, 0xff));
            std::lock_guard<std::mutex> lock(error_replies_mutex);
            error_replies[method] = code;
        } else if (cmd == "quit") {
            return false;
        } else {
            emit("ERR unknown or malformed command: " + line);
            return true;
        }
        emit("OK " + cmd);
    } catch (const std::exception &e) {
        emit(std::string("ERR ") + e.what() + ": " + line);
    }
    return true;
}

}  // namespace

int main() {
    int protocol_fd = dup(STDOUT_FILENO);
    if (protocol_fd < 0 || dup2(STDERR_FILENO, STDOUT_FILENO) < 0 ||
        (protocol_out = fdopen(protocol_fd, "w")) == nullptr) {
        std::perror("redirecting stdout");
        return 1;
    }
    app = vsomeip::runtime::get()->create_application("peer");
    if (!app->init()) {
        std::cerr << "vsomeip init failed" << std::endl;
        return 1;
    }
    app->register_availability_handler(
        vsomeip::ANY_SERVICE, vsomeip::ANY_INSTANCE,
        [](vsomeip::service_t s, vsomeip::instance_t i, bool up) {
            // vsomeip reports a wildcard "unavailable" when the handler is
            // registered; it names no service, so it is not reported.
            if (s == vsomeip::ANY_SERVICE || i == vsomeip::ANY_INSTANCE) return;
            emit(std::string(up ? "AVAILABLE" : "UNAVAILABLE") + " service=" + hex16(s) +
                 " instance=" + hex16(i));
        });
    app->register_subscription_status_handler(
        vsomeip::ANY_SERVICE, vsomeip::ANY_INSTANCE, vsomeip::ANY_EVENTGROUP, vsomeip::ANY_EVENT,
        [](vsomeip::service_t s, vsomeip::instance_t, vsomeip::eventgroup_t eg, vsomeip::event_t,
           uint16_t err) {
            emit("SUBSCRIPTION service=" + hex16(s) + " eventgroup=" + hex16(eg) +
                 " status=" + (err == 0 ? "accepted" : "rejected"));
        });
    app->register_state_handler([](vsomeip::state_type_e st) {
        if (st == vsomeip::state_type_e::ST_REGISTERED) emit("READY");
    });
    std::thread reader([] {
        for (std::string line; std::getline(std::cin, line);) {
            if (!line.empty() && !handle(line)) break;
        }
        app->stop();  // `quit` or stdin closed: the test is done
    });
    app->start();
    reader.join();
    return 0;
}
