cmake_minimum_required(VERSION 3.13)
project(vsomeip_offerer CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# vsomeip's exported config (installed by `make install` in the build
# stage of the Dockerfile) provides the imported targets we need.
find_package(vsomeip3 REQUIRED)

# Two binaries: offerer (advertises the service) and subscriber
# (consumes it via SD availability handler). The Dockerfile builds
# both; the entrypoint dispatches based on `VSOMEIP_ROLE`.
add_executable(offerer offerer.cpp)
add_executable(subscriber subscriber.cpp)

foreach(target offerer subscriber)
    target_link_libraries(${target} PRIVATE vsomeip3)
    target_include_directories(${target} PRIVATE ${VSOMEIP_INCLUDE_DIRS})
endforeach()
