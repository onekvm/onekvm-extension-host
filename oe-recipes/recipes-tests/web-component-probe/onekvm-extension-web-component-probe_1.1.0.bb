SUMMARY = "Temporary OneKVM Web Components acceptance probe"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://LICENSE;md5=8a5a0a588995e3e2c2bfa68f5413df57"
EXCLUDE_FROM_WORLD = "1"
FILESEXTRAPATHS:prepend := "${THISDIR}/../../../tests/fixtures/web-component-probe:${THISDIR}/../../..:"
# Generate the ignored Lit bundle with tests/build-web-component-probe.sh.
SRC_URI = "file://Makefile file://manifest.json.in file://probe.sh file://native-view.js file://console.js file://lit-tool.bundle.js file://LICENSE"
S = "${UNPACKDIR}"
inherit onekvm-extension
OPKGBUILDCMD = "opkg-build -Z gzip"
ONEKVM_EXTENSION_ID = "web-component-probe"
ONEKVM_EXTENSION_API_VERSION = "2"
ONEKVM_EXTENSION_MANIFEST_TEMPLATE = "${S}/manifest.json.in"
RDEPENDS:${PN} += "onekvm-extension-host onekvm-ui"
do_configure[noexec] = "1"
do_compile[noexec] = "1"
do_install() {
    oe_runmake install DESTDIR=${D} PREFIX=${ONEKVM_EXTENSION_ROOT} VERSION=${PV} ARCH=${TARGET_ARCH}
}
