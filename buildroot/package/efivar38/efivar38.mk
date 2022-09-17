################################################################################
#
# efivar38
#
################################################################################


# adapted from buildroot's config.

EFIVAR38_VERSION = 38
EFIVAR38_SOURCE = efivar-38.tar.gz
EFIVAR38_SITE = $(call github,rhboot,efivar,$(EFIVAR38_VERSION))
EFIVAR38_LICENSE = LGPL-2.1
EFIVAR38_LICENSE_FILES = COPYING

# -fPIC is needed at least on MIPS, otherwise fails to build shared
# -library.
EFIVAR38_MAKE_OPTS = \
	libdir=/usr/lib \
	LDFLAGS="$(TARGET_LDFLAGS) -fPIC"

define EFIVAR38_BUILD_CMDS
	sed -i s/mandoc/echo/g $(@D)/src/include/defaults.mk
	$(TARGET_MAKE_ENV) $(TARGET_CONFIGURE_OPTS) $(MAKE1) -C $(@D) \
		AR=$(TARGET_AR) NM=$(TARGET_NM) RANLIB=$(TARGET_RANLIB) \
		$(EFIVAR38_MAKE_OPTS) \
		all
endef

define EFIVAR38_INSTALL_TARGET_CMDS
	$(TARGET_MAKE_ENV) $(TARGET_CONFIGURE_OPTS) $(MAKE1) -C $(@D) \
		$(EFIVAR38_MAKE_OPTS) \
		DESTDIR="$(TARGET_DIR)" \
		install
endef

$(eval $(generic-package))
