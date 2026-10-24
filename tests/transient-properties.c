#define _GNU_SOURCE
#include <assert.h>
#include "../native/bus.c"
int main(void) {
    sd_bus *bus = NULL;
    sd_bus_message *m = NULL;
    assert(sd_bus_open_user(&bus) >= 0);
    assert(sd_bus_message_new_method_call(bus, &m, "org.freedesktop.systemd1", "/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "StartTransientUnit") >= 0);
    assert(sd_bus_message_open_container(m, 'a', "(sv)") >= 0);
    assert(append_transient_property(m, "RestartPreventExitStatus=78") >= 0);
    assert(sd_bus_message_close_container(m) >= 0);
    assert(sd_bus_message_seal(m, 1, 0) >= 0);
    assert(sd_bus_message_rewind(m, 1) >= 0);
    assert(sd_bus_message_enter_container(m, 'a', "(sv)") > 0);
    assert(sd_bus_message_enter_container(m, 'r', "sv") > 0);
    const char *name = NULL;
    assert(sd_bus_message_read(m, "s", &name) > 0);
    assert(strcmp(name, "RestartPreventExitStatus") == 0);
    assert(sd_bus_message_enter_container(m, 'v', "(aiai)") > 0);
    assert(sd_bus_message_enter_container(m, 'r', "aiai") > 0);
    assert(sd_bus_message_enter_container(m, 'a', "i") > 0);
    int32_t code = 0;
    assert(sd_bus_message_read(m, "i", &code) > 0 && code == 78);
    assert(sd_bus_message_read(m, "i", &code) == 0);
    assert(sd_bus_message_exit_container(m) >= 0);
    assert(sd_bus_message_enter_container(m, 'a', "i") > 0);
    assert(sd_bus_message_read(m, "i", &code) == 0);
    sd_bus_message_unref(m);
    assert(sd_bus_message_new_method_call(bus, &m, "org.freedesktop.systemd1", "/org/freedesktop/systemd1", "org.freedesktop.systemd1.Manager", "StartTransientUnit") >= 0);
    assert(append_transient_property(m, "RestartPreventExitStatus=-1") < 0);
    assert(append_transient_property(m, "RestartPreventExitStatus=256") < 0);
    assert(append_transient_property(m, "RestartPreventExitStatus=bad") < 0);
    sd_bus_message_unref(m);
    sd_bus_unref(bus);
    return 0;
}
