################################################################################
#
# pixie-register 
#
################################################################################

PIXIE_REGISTER_VERSION = 2.0
PIXIE_REGISTER_SOURCE = pixie.tar.gz
PIXIE_REGISTER_SITE = $(BR2_EXTERNAL_PIXIE2_PATH)
PIXIE_REGISTER_SITE_METHOD=file
PIXIE_REGISTER_LICENSE = Apache2
PIXIE_REGISTER_LICENSE_FILES = LICENSE 
PIXIE_REGISTER_SUBDIR = pixie-register
PIXIE_REGISTER_DEPENDENCIES = libopenssl host-pkgconf

$(eval $(cargo-package))
