SUMMARY = "Temporary OneKVM extension lifecycle acceptance probe"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://LICENSE;md5=94709c9b19d549c49355fc4fafb70d1a"
EXCLUDE_FROM_WORLD = "1"
FILESEXTRAPATHS:prepend := "${THISDIR}/../../../tests/fixtures/lifecycle-probe:${THISDIR}/../../..:"
SRC_URI = "file://Makefile file://manifest.json.in file://probe.sh file://screen-app file://LICENSE"
S = "${UNPACKDIR}"
inherit onekvm-extension
OPKGBUILDCMD = "opkg-build -Z gzip"
ONEKVM_EXTENSION_ID = "lifecycle-probe"
ONEKVM_EXTENSION_API_VERSION = "2"
ONEKVM_EXTENSION_MANIFEST_TEMPLATE = "${S}/manifest.json.in"
LIFECYCLE_PROBE_FAIL_HEALTH ?= "0"
do_configure[noexec] = "1"
do_compile[noexec] = "1"
do_install() {
    oe_runmake install DESTDIR=${D} PREFIX=${ONEKVM_EXTENSION_ROOT} VERSION=${PV} ARCH=${TARGET_ARCH} FAIL_HEALTH=${LIFECYCLE_PROBE_FAIL_HEALTH}
}
