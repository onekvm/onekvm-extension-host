#include <stdio.h>
#include <systemd/sd-bus.h>

static int unknown_unit(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    (void)userdata;
    (void)error;
    fprintf(stderr, "%s\n", sd_bus_message_get_member(message));
    return sd_bus_reply_method_errorf(message, "org.freedesktop.systemd1.NoSuchUnit",
                                      "The isolated test has no systemd units");
}

static const sd_bus_vtable manager[] = {
    SD_BUS_VTABLE_START(0),
    SD_BUS_METHOD("GetUnit", "s", "o", unknown_unit, SD_BUS_VTABLE_UNPRIVILEGED),
    SD_BUS_METHOD("StopUnit", "ss", "o", unknown_unit, SD_BUS_VTABLE_UNPRIVILEGED),
    SD_BUS_VTABLE_END
};

int main(void) {
    sd_bus *bus = NULL;
    if (sd_bus_open_system(&bus) < 0 ||
        sd_bus_add_object_vtable(bus, NULL, "/org/freedesktop/systemd1",
                                "org.freedesktop.systemd1.Manager", manager, NULL) < 0 ||
        sd_bus_request_name(bus, "org.freedesktop.systemd1", 0) < 0)
        return 1;
    puts("ready");
    fflush(stdout);
    for (;;) {
        int processed = sd_bus_process(bus, NULL);
        if (processed < 0 || (processed == 0 && sd_bus_wait(bus, -1) < 0))
            return 1;
    }
}
