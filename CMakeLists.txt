cmake_minimum_required(VERSION 3.0.2)

set(CMAKE_MODULE_PATH ${CMAKE_MODULE_PATH} "${CMAKE_SOURCE_DIR}/cmake-modules/")

include(CheckCXXCompilerFlag)

option(DEBUG "Enable debug" OFF)
option(BUILD_CLIENT "Build the client" ON)
option(BUILD_SERVER "Build the server" ON)

if(BUILD_SERVER)
project(server)
endif()
if(BUILD_CLIENT)
project(client)
endif()
project(common)

file(GLOB SERVER_SOURCES ${CMAKE_SOURCE_DIR}/server/*.cpp)
file(GLOB CLIENT_SOURCES ${CMAKE_SOURCE_DIR}/client/*.cpp)
file(GLOB COMMON_SOURCES ${CMAKE_SOURCE_DIR}/common/*.cpp)

if(BUILD_SERVER)
    find_package(JSONCPP REQUIRED)
endif()

set(SERVER_LIBS ${SERVER_LIBS} ${JSONCPP_LIBRARIES})
set(SERVER_INCLUDES ${SERVER_INCLUDES} ${JSONCPP_INCLUDES})
set(INCLUDES ${INCLUDES} ${CMAKE_SOURCE_DIR}/include)

if((CMAKE_CXX_COMPILER_ID MATCHES "Clang") OR CMAKE_COMPILER_IS_GNUCXX)
    CHECK_CXX_COMPILER_FLAG("-std=gnu++14" COMPILER_SUPPORTS_GNUXX14)
    if(COMPILER_SUPPORTS_GNUXX14)
        set(FLAGS ${FLAGS} -std=gnu++14)
    else()
        set(FLAGS ${FLAGS} -std=gnu++1y)
    endif()

    if(DEBUG)
        set(FLAGS ${FLAGS} -g)
    else()
        set(FLAGS ${FLAGS} -O3)
    endif()

    set(FLAGS ${FLAGS} -Wall -Wextra)

    set(CLIENT_LIBS ${CLIENT_LIBS} -static -Wl,--no-export-dynamic)

    set(LIBS ${LIBS} -pthread)
endif()

add_library(common STATIC ${COMMON_SOURCES})
target_include_directories(common PRIVATE ${INCLUDES} ${COMMON_INCLUDES})
target_link_libraries(common PRIVATE ${LIBS} ${COMMON_LIBS})
target_compile_options(common PRIVATE ${FLAGS} ${COMMON_FLAGS})
set(LIBS ${LIBS} common)

if(BUILD_CLIENT)
    add_executable(client ${CLIENT_SOURCES})
    target_include_directories(client PRIVATE ${INCLUDES} ${CLIENT_INCLUDES})
    target_link_libraries(client PRIVATE ${LIBS} ${CLIENT_LIBS})
    target_compile_options(client PRIVATE ${FLAGS} ${CLIENT_FLAGS})
endif()

if(BUILD_SERVER)
    add_executable(server ${SERVER_SOURCES})
    target_include_directories(server PRIVATE ${INCLUDES} ${SERVER_INCLUDES})
    target_link_libraries(server PRIVATE ${LIBS} ${SERVER_LIBS})
    target_compile_options(server PRIVATE ${FLAGS} ${SERVER_FLAGS})
endif()
