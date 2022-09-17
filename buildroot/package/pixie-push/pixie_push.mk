################################################################################
#
# pixie-push 
#
################################################################################

PIXIE_PUSH_VERSION = 2.0
PIXIE_PUSH_SOURCE = pixie.tar.gz
PIXIE_PUSH_SITE = $(BR2_EXTERNAL_PIXIE2_PATH)
PIXIE_PUSH_SITE_METHOD=file
PIXIE_PUSH_LICENSE = Apache2
PIXIE_PUSH_LICENSE_FILES = LICENSE 
PIXIE_PUSH_SUBDIR = pixie-push
PIXIE_PUSH_DEPENDENCIES = libopenssl host-pkgconf

$(eval $(cargo-package))
