SUMMARY = "OneKVM extension host and manager CLI"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://LICENSE;md5=8a5a0a588995e3e2c2bfa68f5413df57"
PR = "r13"

ONEKVM_COMPONENT_GIT_URI ??= "git://github.com/onekvm/onekvm-extension-host.git;protocol=https;branch=main"
ONEKVM_COMPONENT_SRCREV ??= ""
SRC_URI = "${ONEKVM_COMPONENT_GIT_URI}"
SRCREV = "${ONEKVM_COMPONENT_SRCREV}"

require onekvm-extension-host-crates.inc

inherit cargo cargo-update-recipe-crates pkgconfig

DEPENDS += "systemd pkgconfig-native"

do_install() {
    install -Dm0755 \
        ${B}/target/${RUST_HOST_SYS}/${BUILD_DIR}/onekvm-extension-host \
        ${D}${bindir}/onekvm-extension-host
    ln -sf onekvm-extension-host ${D}${bindir}/onekvm-extension
}
