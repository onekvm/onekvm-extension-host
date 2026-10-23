SUMMARY = "OneKVM extension host, lifecycle policy, and manager CLI"
LICENSE = "MIT"
LIC_FILES_CHKSUM = "file://LICENSE;md5=8a5a0a588995e3e2c2bfa68f5413df57"
PR = "r37"
OPKGBUILDCMD = "opkg-build -Z gzip"

ONEKVM_TRUSTED_FEED_PREFIXES ?= ""

ONEKVM_COMPONENT_GIT_URI ??= "git://github.com/onekvm/onekvm-extension-host.git;protocol=https;branch=main"
ONEKVM_COMPONENT_SRCREV ??= ""
SRC_URI = "${ONEKVM_COMPONENT_GIT_URI}"
SRCREV = "${ONEKVM_COMPONENT_SRCREV}"

require onekvm-extension-host-crates.inc

inherit cargo cargo-update-recipe-crates pkgconfig systemd useradd

DEPENDS += "systemd pkgconfig-native"

do_install() {
    install -Dm0755 \
        ${B}/target/${RUST_HOST_SYS}/${BUILD_DIR}/onekvm-extension-host \
        ${D}${bindir}/onekvm-extension-host
    ln -sf onekvm-extension-host ${D}${bindir}/onekvm-extension
    ln -sf onekvm-extension-host ${D}${bindir}/onekvm-extension-manager
    ln -sf onekvm-extension-host ${D}${bindir}/onekvm-plugin-manager

    install -Dm0644 ${S}/packaging/onekvm-extension-host.service \
        ${D}${systemd_system_unitdir}/onekvm-extension-host.service
    install -Dm0644 ${S}/packaging/onekvm-plugin-update.service \
        ${D}${systemd_system_unitdir}/onekvm-plugin-update.service
    install -d ${D}${systemd_system_unitdir}/onekvm-pre.target.requires
    ln -s ../onekvm-plugin-update.service \
        ${D}${systemd_system_unitdir}/onekvm-pre.target.requires/onekvm-plugin-update.service
    install -d ${D}${datadir}/onekvm
    printf '1\n' >${D}${datadir}/onekvm/plugin-update-version
    install -Dm0644 ${S}/packaging/30-onekvm-extension-group.conf \
        ${D}${systemd_system_unitdir}/onekvm.service.d/30-onekvm-extension-group.conf
    install -Dm0644 ${S}/packaging/system-onekvm.slice \
        ${D}${systemd_system_unitdir}/system-onekvm.slice
    install -Dm0644 ${S}/packaging/system-onekvm-extensions.slice \
        ${D}${systemd_system_unitdir}/system-onekvm-extensions.slice
    install -Dm0644 ${S}/packaging/org.onekvm.ExtensionHost1.conf \
        ${D}${datadir}/dbus-1/system.d/org.onekvm.ExtensionHost1.conf
    install -Dm0644 ${S}/packaging/org.onekvm.ExtensionHost1.service \
        ${D}${datadir}/dbus-1/system-services/org.onekvm.ExtensionHost1.service

    install -Dm0644 ${S}/docs/extension-controller-v1.md \
        ${D}${docdir}/onekvm/extension-controller-v1.md
    install -Dm0644 ${S}/docs/extension-command-method-v1.md \
        ${D}${docdir}/onekvm/extension-command-method-v1.md
    install -Dm0644 ${S}/docs/extension-points.md \
        ${D}${docdir}/onekvm/extension-points.md
    install -Dm0644 ${S}/docs/extension-logging-v1.md \
        ${D}${docdir}/onekvm/extension-logging-v1.md
    install -Dm0644 ${S}/docs/extension-toolbox-v1.md \
        ${D}${docdir}/onekvm/extension-toolbox-v1.md
    install -Dm0644 ${S}/docs/ipks-v1.md \
        ${D}${docdir}/onekvm/ipks-v1.md

    install -d ${D}${datadir}/onekvm/extension-compat
    install -m0644 ${S}/packaging/onekvm-token ${D}${datadir}/onekvm/extension-compat/onekvm-token
    if [ -n "${ONEKVM_TRUSTED_FEED_PREFIXES}" ]; then
        : >${D}${datadir}/onekvm/trusted-feed-prefixes
        for prefix in ${ONEKVM_TRUSTED_FEED_PREFIXES}; do
            printf '%s\n' "$prefix" >>${D}${datadir}/onekvm/trusted-feed-prefixes
        done
        chmod 0644 ${D}${datadir}/onekvm/trusted-feed-prefixes
    else
        install -m0644 ${S}/packaging/trusted-feed-prefixes \
            ${D}${datadir}/onekvm/trusted-feed-prefixes
    fi
}

SYSTEMD_SERVICE:${PN} = "onekvm-extension-host.service"
SYSTEMD_AUTO_ENABLE:${PN} = "disable"

RDEPENDS:${PN} += "opkg systemd gnupg-gpg"
RPROVIDES:${PN} += "onekvm-plugin-manager"
RREPLACES:${PN} += "onekvm-plugin-manager"
RCONFLICTS:${PN} += "onekvm-plugin-manager"

FILES:${PN} += "\
    ${systemd_system_unitdir}/onekvm-plugin-update.service \
    ${systemd_system_unitdir}/onekvm-pre.target.requires \
    ${datadir}/onekvm \
    ${datadir}/dbus-1/system.d/org.onekvm.ExtensionHost1.conf \
    ${datadir}/dbus-1/system-services/org.onekvm.ExtensionHost1.service \
    ${docdir}/onekvm \
    ${systemd_system_unitdir}/system-onekvm.slice \
    ${systemd_system_unitdir}/system-onekvm-extensions.slice \
    ${systemd_system_unitdir}/onekvm.service.d/30-onekvm-extension-group.conf \
"

USERADD_PACKAGES = "${PN}"
USERADD_PARAM:${PN} = "--system --home /nonexistent --no-create-home --shell /sbin/nologin onekvm-extension"

GROUPADD_PARAM:${PN} = "--system onekvm-crypto"
