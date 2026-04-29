cmake_minimum_required(VERSION 3.13)
project(vsomeip_offerer CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

# vsomeip's exported config (installed by `make install` in the build
# stage of the Dockerfile) provides the imported targets we need.
find_package(vsomeip3 REQUIRED)

# Offerer binary: tiny, links libvsomeip3.
add_executable(offerer offerer.cpp)

target_link_libraries(offerer
    PRIVATE
        vsomeip3
)

# vsomeip publishes its headers under <vsomeip/...>.
target_include_directories(offerer
    PRIVATE
        ${VSOMEIP_INCLUDE_DIRS}
)
