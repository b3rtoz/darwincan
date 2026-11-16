# Builds darwinCAN and installs the daemon, the tools and the C library.
#
#     make                  build everything (cargo build --release)
#     make check            run the tests
#     sudo make install     into /usr/local, or: make install PREFIX=$HOME/.local
#     sudo make uninstall
#
# After `make install` into /usr/local a C program needs nothing more than
#     #include <darwincan.h>          and          cc prog.c -ldarwincan
# and with any other prefix
#     cc prog.c $(PKG_CONFIG_PATH=<prefix>/lib/pkgconfig pkg-config --cflags --libs darwincan)

PREFIX ?= /usr/local
DESTDIR ?=
CARGO ?= cargo

bindir = $(DESTDIR)$(PREFIX)/bin
libdir = $(DESTDIR)$(PREFIX)/lib
includedir = $(DESTDIR)$(PREFIX)/include
release = target/release

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
PROGRAMS = darwincand candump cansend cangen canplayer isotpsend isotprecv canctl \
	j1939acd j1939spy j1939cat j1939sr testj1939

.PHONY: all check install uninstall

all:
	$(CARGO) build --release

check:
	$(CARGO) test

install: all
	install -d $(bindir) $(libdir)/pkgconfig $(includedir)
	for p in $(PROGRAMS); do install -m 755 $(release)/$$p $(bindir)/ || exit 1; done
	install -m 644 crates/darwincan/include/darwincan.h $(includedir)/
	install -m 644 crates/darwincan/include/darwincan_bcm.h $(includedir)/
	install -m 644 crates/darwincan/include/darwincan_j1939.h $(includedir)/
	install -m 644 $(release)/libdarwincan.a $(libdir)/
	install -m 755 $(release)/libdarwincan.dylib $(libdir)/
	@# The library is built knowing only where the build left it; tell it
	@# where it lives now, so programs linked against it find it there.
	install_name_tool -id $(PREFIX)/lib/libdarwincan.dylib $(libdir)/libdarwincan.dylib
	codesign --force --sign - $(libdir)/libdarwincan.dylib
	printf '%s\n' \
		'prefix=$(PREFIX)' \
		'libdir=$${prefix}/lib' \
		'includedir=$${prefix}/include' \
		'' \
		'Name: darwincan' \
		'Description: CAN bus access on macOS through the darwincand daemon' \
		'Version: $(VERSION)' \
		'Libs: -L$${libdir} -ldarwincan' \
		'Cflags: -I$${includedir}' \
		> $(libdir)/pkgconfig/darwincan.pc

uninstall:
	for p in $(PROGRAMS); do rm -f $(bindir)/$$p; done
	rm -f $(includedir)/darwincan.h $(includedir)/darwincan_bcm.h \
		$(includedir)/darwincan_j1939.h \
		$(libdir)/libdarwincan.a \
		$(libdir)/libdarwincan.dylib $(libdir)/pkgconfig/darwincan.pc
