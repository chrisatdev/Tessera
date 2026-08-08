# Tessera — installation
#
# Usage:
#   make install PREFIX=/usr
#     Installs the tessera binary to $(BINDIR) and the XDG session entry to
#     $(PREFIX)/share/xsessions so GDM, SDDM, and LightDM list "Tessera" as a
#     selectable login session. PREFIX defaults to /usr.
#
#   make install PREFIX=/usr DESTDIR=/tmp/stage
#     Staged install for packaging (Arch PKGBUILD style): every path is
#     prefixed with $(DESTDIR). Set DESTDIR to the package staging root.

PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
DESTDIR ?=

.PHONY: all build install help

all: build

build:
	cargo build --release

install: build
	install -d $(DESTDIR)$(PREFIX)/share/xsessions $(DESTDIR)$(BINDIR)
	install -m 0644 install/tessera.desktop $(DESTDIR)$(PREFIX)/share/xsessions/tessera.desktop
	install -m 0755 target/release/tessera $(DESTDIR)$(BINDIR)/tessera

help:
	@echo "Tessera installation"
	@echo "  make install PREFIX=/usr"
	@echo "    install the tessera binary to $(BINDIR) and the XDG session"
	@echo "    entry to $(PREFIX)/share/xsessions so display managers list"
	@echo "    Tessera as a login session."
	@echo "  make install PREFIX=/usr DESTDIR=/tmp/stage"
	@echo "    staged install for packaging (PKGBUILD style)."
