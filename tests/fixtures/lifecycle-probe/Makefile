DESTDIR ?=
PREFIX ?= /usr/local
VERSION ?= 1.0.0
ARCH ?= unknown
FAIL_HEALTH ?= 0

install:
	install -d $(DESTDIR)$(PREFIX)/bin $(DESTDIR)$(PREFIX)/screen-app
	sed -e 's/@VERSION@/$(VERSION)/g' -e 's/@FAIL_HEALTH@/$(FAIL_HEALTH)/g' probe.sh > $(DESTDIR)$(PREFIX)/bin/probe
	chmod 0755 $(DESTDIR)$(PREFIX)/bin/probe
	sed -e 's/@VERSION@/$(VERSION)/g' -e 's/@ARCH@/$(ARCH)/g' manifest.json.in > $(DESTDIR)$(PREFIX)/manifest.json
	install -m0644 screen-app/* $(DESTDIR)$(PREFIX)/screen-app/
