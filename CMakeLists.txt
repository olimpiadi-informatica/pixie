cmake_minimum_required(VERSION 3.0.2)

set(CMAKE_MODULE_PATH ${CMAKE_MODULE_PATH} "${CMAKE_SOURCE_DIR}/cmake-modules/")

include(CheckCXXCompilerFlag)

option(DEBUG "Enable Debug" OFF)

project(server)
project(client)

file(GLOB SERVER_SOURCES ${CMAKE_SOURCE_DIR}/server/*.cpp)
file(GLOB CLIENT_SOURCES ${CMAKE_SOURCE_DIR}/client/*.cpp)

find_package(JSONCPP REQUIRED)

set(SERVER_LIBS ${SERVER_LIBS} ${JSONCPP_LIBRARIES})
set(SERVER_INCLUDES ${SERVER_LIBS} ${JSONCPP_INCLUDES})
set(INCLUDES ${INCLUDES} ${CMAKE_SOURCE_DIR}/include)

if((CMAKE_CXX_COMPILER_ID MATCHES "Clang") OR CMAKE_COMPILER_IS_GNUCXX)
    CHECK_CXX_COMPILER_FLAG("-std=c++14" COMPILER_SUPPORTS_CXX14)
    if(COMPILER_SUPPORTS_CXX14)
        set(FLAGS ${FLAGS} -std=c++14)
    else()
        set(FLAGS ${FLAGS} -std=c++1y)
    endif()

    if(DEBUG)
        set(FLAGS ${FLAGS} -g)
    else()
        set(FLAGS ${FLAGS} -O3)
    endif()

    set(FLAGS ${FLAGS} -Wall -Wextra)

    set(CLIENT_LIBS ${CLIENT_LIBS} -static)
endif()

add_executable(client ${CLIENT_SOURCES})
add_executable(server ${SERVER_SOURCES})

target_include_directories(client PRIVATE ${INCLUDES} ${CLIENT_INCLUDES})
target_include_directories(server PRIVATE ${INCLUDES} ${SERVER_INCLUDES})

target_link_libraries(client PRIVATE ${LIBS} ${CLIENT_LIBS})
target_link_libraries(server PRIVATE ${LIBS} ${SERVER_LIBS})

target_compile_options(client PRIVATE ${FLAGS} ${CLIENT_FLAGS})
target_compile_options(server PRIVATE ${FLAGS} ${SERVER_FLAGS})
