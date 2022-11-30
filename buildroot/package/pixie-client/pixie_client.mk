################################################################################
#
# pixie-client
#
################################################################################

PIXIE_CLIENT_VERSION = 2.0
PIXIE_CLIENT_SOURCE = pixie.tar.gz
PIXIE_CLIENT_SITE = $(BR2_EXTERNAL_PIXIE2_PATH)
PIXIE_CLIENT_SITE_METHOD=file
PIXIE_CLIENT_LICENSE = Apache2
PIXIE_CLIENT_LICENSE_FILES = LICENSE
PIXIE_CLIENT_SUBDIR = pixie-client
PIXIE_CLIENT_DEPENDENCIES = libopenssl host-pkgconf

$(eval $(cargo-package))
