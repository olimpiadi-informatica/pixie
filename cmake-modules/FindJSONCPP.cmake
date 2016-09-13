find_path(JSONCPP_INCLUDES "json/json.h"
          HINTS ${CMAKE_INSTALL_PREFIX}
          PATH_SUFFIXES include)

find_library(JSONCPP_LIBRARIES "jsoncpp"
             HINTS ${CMAKE_INSTALL_PREFIX}
             PATH_SUFFIXES lib lib64 lib32)

if(JSONCPP_INCLUDES AND JSONCPP_LIBRARIES)
    set(JSONCPP_FOUND TRUE)
else()
    set(JSONCPP_FOUND FALSE)
endif()

include(FindPackageHandleStandardArgs)

find_package_handle_standard_args(JSONCPP DEFAULT_MSG JSONCPP_LIBRARIES JSONCPP_INCLUDES)
