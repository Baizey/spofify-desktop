# Install a built Linux binary; DESTDIR can stage a distribution package.
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/bin
DATADIR ?= $(PREFIX)/share
BINARY ?= $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),target)/release/spofify-desktop

.PHONY: build test install uninstall
build:
	cargo build --release --locked

test:
	cargo test --locked

install:
	install -Dm755 "$(BINARY)" "$(DESTDIR)$(BINDIR)/spofify-desktop"
	install -Dm644 packaging/spofify-desktop.desktop "$(DESTDIR)$(DATADIR)/applications/spofify-desktop.desktop"
	install -Dm644 assets/spofify-logo.svg "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/spofify-desktop.svg"
	install -Dm644 assets/JetBrainsMono-OFL.txt "$(DESTDIR)$(DATADIR)/licenses/spofify-desktop/JetBrainsMono-OFL.txt"

uninstall:
	$(RM) "$(DESTDIR)$(BINDIR)/spofify-desktop"
	$(RM) "$(DESTDIR)$(DATADIR)/applications/spofify-desktop.desktop"
	$(RM) "$(DESTDIR)$(DATADIR)/icons/hicolor/scalable/apps/spofify-desktop.svg"
	$(RM) "$(DESTDIR)$(DATADIR)/licenses/spofify-desktop/JetBrainsMono-OFL.txt"
