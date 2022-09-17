################################################################################
#
# pixie-pull 
#
################################################################################

PIXIE_PULL_VERSION = 2.0
PIXIE_PULL_SOURCE = pixie.tar.gz
PIXIE_PULL_SITE = $(BR2_EXTERNAL_PIXIE2_PATH)
PIXIE_PULL_SITE_METHOD=file
PIXIE_PULL_LICENSE = Apache2
PIXIE_PULL_LICENSE_FILES = LICENSE 
PIXIE_PULL_SUBDIR = pixie-pull
PIXIE_PULL_DEPENDENCIES = libopenssl host-pkgconf

$(eval $(cargo-package))
