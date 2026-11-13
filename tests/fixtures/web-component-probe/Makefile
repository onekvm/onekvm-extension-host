DESTDIR ?=
PREFIX ?= /usr/local
VERSION ?= 1.1.0
ARCH ?= unknown

install:
	install -Dm0755 probe.sh $(DESTDIR)$(PREFIX)/bin/probe
	install -Dm0644 native-view.js $(DESTDIR)$(PREFIX)/web/page.js
	install -Dm0644 native-view.js $(DESTDIR)$(PREFIX)/web/native-tool.js
	install -Dm0644 lit-tool.bundle.js $(DESTDIR)$(PREFIX)/web/lit-tool.js
	install -Dm0644 console.js $(DESTDIR)$(PREFIX)/web/console.js
	sed -e 's/@VERSION@/$(VERSION)/g' -e 's/@ARCH@/$(ARCH)/g' manifest.json.in > $(DESTDIR)$(PREFIX)/manifest.json
	chmod 0644 $(DESTDIR)$(PREFIX)/manifest.json
