#define _GNU_SOURCE
#include <errno.h>
#include <linux/capability.h>
#include <limits.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <systemd/sd-bus.h>
#include <systemd/sd-daemon.h>
#include <time.h>

#define HOST_NAME "org.onekvm.ExtensionHost1"
#define HOST_PATH "/org/onekvm/ExtensionHost1"
#define HOST_IFACE "org.onekvm.ExtensionHost1"
#define HOST_ERR "org.onekvm.ExtensionHost1.Error.Failed"
#define HELPER_PATH "/org/onekvm/ExtensionHelper1"
#define HELPER_IFACE "org.onekvm.ExtensionHelper1"
#define CORE_NAME "org.onekvm.Core1"
#define SYSTEMD_NAME "org.freedesktop.systemd1"
#define SYSTEMD_PATH "/org/freedesktop/systemd1"
#define SYSTEMD_MANAGER "org.freedesktop.systemd1.Manager"
#define SYSTEMD_UNIT "org.freedesktop.systemd1.Unit"
#define SYSTEMD_NO_SUCH_UNIT "org.freedesktop.systemd1.NoSuchUnit"

#define UNIT_STATUS_ACTIVE  (1u << 0)
#define UNIT_STATUS_FAILED  (1u << 1)
#define UNIT_STATUS_ENABLED (1u << 2)

int onekvm_host_apply_config(void *host, const char *id, uint64_t revision,
                             const char *settings, char **keys, size_t nkeys,
                             char **err);
int onekvm_host_simple(void *host, const char *method, const char *id,
                       uint64_t revision, char **err);
int onekvm_host_prewarm(void *host, char **err);
int onekvm_host_reconcile_all(void *host, const char *generation, char **err);
int onekvm_host_invoke(void *host, const char *id, const char *method,
                       const char *request, char **response, char **err);
void onekvm_host_core_lost(void *host);

int onekvm_helper_set_address(void *helper, const char *name);
int onekvm_helper_ping(void *helper, char **id, char **err);
int onekvm_helper_run_controller(void *helper, const void *payload, size_t size,
                                 void **output, size_t *out_size, char **err);
int onekvm_helper_ensure(void *helper, const char *name, char **command,
                         size_t ncommand, char **environment, size_t nenv,
                         const char *restart, char **err);
int onekvm_helper_remove(void *helper, const char *name, char **err);
int onekvm_helper_remove_all(void *helper, char **err);
int onekvm_helper_restart(void *helper, const char *name, char **err);
int onekvm_helper_exec(void *helper, char **command, size_t ncommand,
                       char **environment, size_t nenv, uint32_t timeout,
                       char **err);
int onekvm_helper_exec_capture(void *helper, char **command, size_t ncommand,
                               char **environment, size_t nenv,
                               const void *payload, size_t size,
                               uint32_t timeout, void **output,
                               size_t *out_size, char **err);
int onekvm_helper_instances(void *helper, char ***names, size_t *count,
                            char **err);
int onekvm_helper_should_exit(void *helper);

static uint64_t now_usec(void) {
    struct timespec ts;
    if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0)
        return 0;
    return (uint64_t)ts.tv_sec * 1000000ull + (uint64_t)ts.tv_nsec / 1000ull;
}

static int fail(sd_bus_error *error, char *message) {
    const char *text = message && message[0] ? message : "extension host failed";
    int result = sd_bus_error_setf(error, HOST_ERR, "%s", text);
    free(message);
    return result < 0 ? result : -EIO;
}

static int read_string_array(sd_bus_message *message, char ***out, size_t *count) {
    size_t cap = 0, n = 0;
    char **values = NULL;
    int result = sd_bus_message_enter_container(message, 'a', "s");
    if (result < 0)
        return result;
    for (;;) {
        const char *value = NULL;
        result = sd_bus_message_read(message, "s", &value);
        if (result < 0)
            goto fail;
        if (result == 0)
            break;
        if (n == cap) {
            cap = cap ? cap * 2 : 4;
            char **grown = realloc(values, cap * sizeof(*grown));
            if (!grown) {
                result = -ENOMEM;
                goto fail;
            }
            values = grown;
        }
        values[n] = strdup(value ? value : "");
        if (!values[n]) {
            result = -ENOMEM;
            goto fail;
        }
        n++;
    }
    result = sd_bus_message_exit_container(message);
    if (result < 0)
        goto fail;
    *out = values;
    *count = n;
    return 0;
fail:
    for (size_t i = 0; i < n; i++)
        free(values[i]);
    free(values);
    return result;
}

static void free_string_array(char **values, size_t count) {
    if (!values)
        return;
    for (size_t i = 0; i < count; i++)
        free(values[i]);
    free(values);
}

static int append_string_array(sd_bus_message *message, char **values, size_t count);

static int append_property_strv(sd_bus_message *message, const char *name,
                                const char *value) {
    char *copy = strdup(value ? value : "");
    if (!copy)
        return -ENOMEM;
    int result = sd_bus_message_open_container(message, 'r', "sv");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "s", name);
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'v', "as");
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'a', "s");
    if (result < 0)
        goto out;
    char *state = NULL;
    for (char *word = strtok_r(copy, " \t", &state); word;
         word = strtok_r(NULL, " \t", &state)) {
        result = sd_bus_message_append(message, "s", word);
        if (result < 0)
            goto out;
    }
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
out:
    free(copy);
    return result;
}

static int parse_boolean(const char *value, int *out) {
    if (!value || !out)
        return -EINVAL;
    if (strcmp(value, "yes") == 0 || strcmp(value, "true") == 0 ||
        strcmp(value, "1") == 0) {
        *out = 1;
        return 0;
    }
    if (strcmp(value, "no") == 0 || strcmp(value, "false") == 0 ||
        strcmp(value, "0") == 0) {
        *out = 0;
        return 0;
    }
    return -EINVAL;
}

static int parse_unsigned(const char *value, int base, uint64_t *out) {
    if (!value || !value[0] || !out || value[0] == '-')
        return -EINVAL;
    errno = 0;
    char *end = NULL;
    unsigned long long parsed = strtoull(value, &end, base);
    if (errno || !end || end == value || *end)
        return -EINVAL;
    *out = (uint64_t)parsed;
    return 0;
}

static int parse_size(const char *value, uint64_t *out) {
    if (!value || !value[0] || !out || value[0] == '-')
        return -EINVAL;
    errno = 0;
    char *end = NULL;
    unsigned long long parsed = strtoull(value, &end, 10);
    if (errno || !end || end == value)
        return -EINVAL;
    uint64_t multiplier = 1;
    if (end[0] && !end[1]) {
        switch (end[0]) {
        case 'K': case 'k': multiplier = 1024ull; break;
        case 'M': case 'm': multiplier = 1024ull * 1024ull; break;
        case 'G': case 'g': multiplier = 1024ull * 1024ull * 1024ull; break;
        default: return -EINVAL;
        }
    } else if (end[0])
        return -EINVAL;
    if ((uint64_t)parsed > UINT64_MAX / multiplier)
        return -ERANGE;
    *out = (uint64_t)parsed * multiplier;
    return 0;
}

static int parse_usec(const char *value, uint64_t *out) {
    if (!value || !value[0] || !out || value[0] == '-')
        return -EINVAL;
    errno = 0;
    char *end = NULL;
    unsigned long long parsed = strtoull(value, &end, 10);
    if (errno || !end || end == value)
        return -EINVAL;
    uint64_t multiplier;
    if (strcmp(end, "s") == 0)
        multiplier = 1000000ull;
    else if (strcmp(end, "ms") == 0)
        multiplier = 1000ull;
    else if (strcmp(end, "us") == 0)
        multiplier = 1;
    else
        return -EINVAL;
    if ((uint64_t)parsed > UINT64_MAX / multiplier)
        return -ERANGE;
    *out = (uint64_t)parsed * multiplier;
    return 0;
}

static int append_filter_property(sd_bus_message *message, const char *name,
                                  const char *value) {
    int allow = 1;
    if (value && value[0] == '~') {
        allow = 0;
        value++;
    }
    char *copy = strdup(value ? value : "");
    if (!copy)
        return -ENOMEM;
    int result = sd_bus_message_open_container(message, 'r', "sv");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "s", name);
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'v', "(bas)");
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'r', "bas");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "b", allow);
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'a', "s");
    if (result < 0)
        goto out;
    char *state = NULL;
    for (char *word = strtok_r(copy, " \t", &state); word;
         word = strtok_r(NULL, " \t", &state)) {
        result = sd_bus_message_append(message, "s", word);
        if (result < 0)
            goto out;
    }
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
out:
    free(copy);
    return result;
}

static int append_bind_paths_property(sd_bus_message *message, const char *name,
                                      const char *value) {
    char *copy = strdup(value ? value : "");
    if (!copy)
        return -ENOMEM;
    int result = sd_bus_message_open_container(message, 'r', "sv");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "s", name);
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'v', "a(ssbt)");
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'a', "(ssbt)");
    if (result < 0)
        goto out;
    char *state = NULL;
    for (char *word = strtok_r(copy, " \t", &state); word;
         word = strtok_r(NULL, " \t", &state)) {
        char *destination = strchr(word, ':');
        const char *source = word;
        if (destination) {
            *destination++ = '\0';
            if (!source[0] || !destination[0] || strchr(destination, ':')) {
                result = -EINVAL;
                goto out;
            }
        } else {
            destination = word;
        }
        /* (source, destination, ignore_enoent, recursive) */
        result = sd_bus_message_append(message, "(ssbt)", source, destination, 0, 0);
        if (result < 0)
            goto out;
    }
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
out:
    free(copy);
    return result;
}

static int append_pair_array_property(sd_bus_message *message, const char *name,
                                      const char *first, const char *second) {
    return sd_bus_message_append(message, "(sv)", name, "a(ss)", 1,
                                 first ? first : "", second ? second : "");
}

static int append_capabilities(sd_bus_message *message, const char *name,
                               const char *value) {
    char *copy = strdup(value ? value : "");
    if (!copy)
        return -ENOMEM;
    uint64_t mask = 0;
    char *state = NULL;
    int result = 0;
    for (char *word = strtok_r(copy, " \t", &state); word;
         word = strtok_r(NULL, " \t", &state)) {
        unsigned capability;
        if (strcmp(word, "CAP_SETGID") == 0)
            capability = CAP_SETGID;
        else if (strcmp(word, "CAP_SETUID") == 0)
            capability = CAP_SETUID;
        else if (strcmp(word, "CAP_NET_ADMIN") == 0)
            capability = CAP_NET_ADMIN;
        else if (strcmp(word, "CAP_NET_RAW") == 0)
            capability = CAP_NET_RAW;
        else {
            result = -EINVAL;
            goto out;
        }
        mask |= 1ull << capability;
    }
    result = sd_bus_message_append(message, "(sv)", name, "t", mask);
out:
    free(copy);
    return result;
}

static int append_transient_property(sd_bus_message *message,
                                     const char *assignment) {
    const char *separator = assignment ? strchr(assignment, '=') : NULL;
    if (!separator || separator == assignment)
        return -EINVAL;
    char *name = strndup(assignment, (size_t)(separator - assignment));
    if (!name)
        return -ENOMEM;
    const char *value = separator + 1;
    int result;

    if (strcmp(name, "Type") == 0 || strcmp(name, "User") == 0 ||
        strcmp(name, "Group") == 0 || strcmp(name, "ProtectSystem") == 0 ||
        strcmp(name, "ProtectHome") == 0 || strcmp(name, "DevicePolicy") == 0 ||
        strcmp(name, "Restart") == 0 || strcmp(name, "KillMode") == 0 ||
        strcmp(name, "CollectMode") == 0) {
        result = sd_bus_message_append(message, "(sv)", name, "s", value);
    } else if (strcmp(name, "NoNewPrivileges") == 0 ||
               strcmp(name, "PrivateTmp") == 0 ||
               strcmp(name, "ProtectKernelTunables") == 0 ||
               strcmp(name, "ProtectKernelModules") == 0 ||
               strcmp(name, "ProtectKernelLogs") == 0 ||
               strcmp(name, "ProtectControlGroups") == 0 ||
               strcmp(name, "ProtectHostname") == 0 ||
               strcmp(name, "RestrictSUIDSGID") == 0 ||
               strcmp(name, "LockPersonality") == 0 ||
               strcmp(name, "RestrictRealtime") == 0 ||
               strcmp(name, "PrivateNetwork") == 0 ||
               strcmp(name, "PrivateDevices") == 0 ||
               strcmp(name, "ProtectClock") == 0 ||
               strcmp(name, "Delegate") == 0) {
        int boolean;
        result = parse_boolean(value, &boolean);
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)", name, "b", boolean);
    } else if (strcmp(name, "BindPaths") == 0 ||
               strcmp(name, "BindReadOnlyPaths") == 0) {
        result = append_bind_paths_property(message, name, value);
    } else if (strcmp(name, "ReadOnlyPaths") == 0 ||
               strcmp(name, "ReadWritePaths") == 0 ||
               strcmp(name, "SupplementaryGroups") == 0 ||
               strcmp(name, "SystemCallArchitectures") == 0 ||
               strcmp(name, "BindsTo") == 0 || strcmp(name, "PartOf") == 0 ||
               strcmp(name, "After") == 0) {
        result = append_property_strv(message, name, value);
    } else if (strcmp(name, "RestrictAddressFamilies") == 0) {
        result = append_filter_property(message, name, value);
    } else if (strcmp(name, "CapabilityBoundingSet") == 0 ||
               strcmp(name, "AmbientCapabilities") == 0) {
        result = append_capabilities(message, name, value);
    } else if (strcmp(name, "UMask") == 0) {
        uint64_t mode;
        result = parse_unsigned(value, 8, &mode);
        if (result >= 0 && mode > UINT_MAX)
            result = -ERANGE;
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)", name, "u", (uint32_t)mode);
    } else if (strcmp(name, "MemoryMax") == 0) {
        uint64_t limit;
        result = parse_size(value, &limit);
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)", name, "t", limit);
    } else if (strcmp(name, "TasksMax") == 0) {
        uint64_t limit;
        result = parse_unsigned(value, 10, &limit);
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)", name, "t", limit);
    } else if (strcmp(name, "CPUQuota") == 0) {
        size_t length = strlen(value);
        uint64_t percent;
        if (length < 2 || value[length - 1] != '%')
            result = -EINVAL;
        else {
            char *number = strndup(value, length - 1);
            if (!number)
                result = -ENOMEM;
            else {
                result = parse_unsigned(number, 10, &percent);
                free(number);
                if (result >= 0 && (percent == 0 || percent > UINT64_MAX / 10000ull))
                    result = -ERANGE;
                if (result >= 0)
                    result = sd_bus_message_append(message, "(sv)",
                                                   "CPUQuotaPerSecUSec", "t",
                                                   percent * 10000ull);
            }
        }
    } else if (strcmp(name, "LimitNOFILE") == 0) {
        uint64_t limit;
        result = parse_unsigned(value, 10, &limit);
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)(sv)",
                                           "LimitNOFILE", "t", limit,
                                           "LimitNOFILESoft", "t", limit);
    } else if (strcmp(name, "RestartPreventExitStatus") == 0) {
        /* The D-Bus property is (exit-status[], signal[]), not a string.
         * Legacy controllers reserve EX_CONFIG=78 for operator repair. */
        uint64_t code;
        result = parse_unsigned(value, 10, &code);
        if (result >= 0 && code > 255)
            result = -ERANGE;
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)", name, "(aiai)",
                                           1, (int32_t)code, 0);
    } else if (strcmp(name, "RestartSec") == 0 ||
               strcmp(name, "TimeoutStopSec") == 0) {
        uint64_t usec;
        result = parse_usec(value, &usec);
        if (result >= 0)
            result = sd_bus_message_append(message, "(sv)",
                                           strcmp(name, "RestartSec") == 0
                                               ? "RestartUSec" : "TimeoutStopUSec",
                                           "t", usec);
    } else if (strcmp(name, "LoadCredential") == 0) {
        const char *colon = strchr(value, ':');
        if (!colon || colon == value || !colon[1])
            result = -EINVAL;
        else {
            char *credential = strndup(value, (size_t)(colon - value));
            if (!credential)
                result = -ENOMEM;
            else {
                result = append_pair_array_property(message, name, credential, colon + 1);
                free(credential);
            }
        }
    } else if (strcmp(name, "DeviceAllow") == 0) {
        const char *space = strchr(value, ' ');
        if (!space || space == value)
            result = append_pair_array_property(message, name, value, "");
        else {
            char *device = strndup(value, (size_t)(space - value));
            if (!device)
                result = -ENOMEM;
            else {
                while (*space == ' ' || *space == '\t')
                    space++;
                result = append_pair_array_property(message, name, device, space);
                free(device);
            }
        }
    } else
        result = -EINVAL;

    free(name);
    return result;
}

static int wait_for_unit_start(sd_bus *bus, const char *unit) {
    const uint64_t deadline = now_usec() + 35000000ull;
    for (;;) {
        sd_bus_message *reply = NULL;
        sd_bus_error error = SD_BUS_ERROR_NULL;
        char *state = NULL;
        const char *path = NULL;
        int result = sd_bus_call_method(bus, "org.freedesktop.systemd1",
                                        "/org/freedesktop/systemd1",
                                        "org.freedesktop.systemd1.Manager", "GetUnit",
                                        &error, &reply, "s", unit);
        if (result >= 0)
            result = sd_bus_message_read(reply, "o", &path);
        if (result >= 0)
            result = sd_bus_get_property_string(bus, "org.freedesktop.systemd1", path,
                                                "org.freedesktop.systemd1.Unit",
                                                "ActiveState", &error, &state);
        if (result >= 0 &&
            (strcmp(state, "active") == 0 || strcmp(state, "reloading") == 0))
            result = 0;
        else if (result >= 0 && strcmp(state, "failed") == 0)
            result = -EIO;
        else if (now_usec() >= deadline)
            result = -ETIMEDOUT;
        else
            result = -EAGAIN;
        free(state);
        sd_bus_error_free(&error);
        sd_bus_message_unref(reply);
        if (result != -EAGAIN)
            return result;
        struct timespec delay = { .tv_sec = 0, .tv_nsec = 25000000 };
        while (nanosleep(&delay, &delay) < 0 && errno == EINTR)
            ;
    }
}

static int systemd_error_is_missing(const sd_bus_error *error, int result) {
    return result == -ENOENT || result == -ENXIO ||
           sd_bus_error_has_name(error, SYSTEMD_NO_SUCH_UNIT) ||
           sd_bus_error_has_name(error, SD_BUS_ERROR_UNKNOWN_OBJECT);
}

static int wait_for_unit_stop(sd_bus *bus, const char *unit) {
    const uint64_t deadline = now_usec() + 35000000ull;
    for (;;) {
        sd_bus_message *reply = NULL;
        sd_bus_error error = SD_BUS_ERROR_NULL;
        char *state = NULL;
        const char *path = NULL;
        int result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                        SYSTEMD_MANAGER, "GetUnit",
                                        &error, &reply, "s", unit);
        if (systemd_error_is_missing(&error, result))
            result = 0;
        else {
            if (result >= 0)
                result = sd_bus_message_read(reply, "o", &path);
            if (result >= 0)
                result = sd_bus_get_property_string(bus, SYSTEMD_NAME, path,
                                                    SYSTEMD_UNIT, "ActiveState",
                                                    &error, &state);
            if (result >= 0 && strcmp(state, "inactive") == 0)
                result = 0;
            else if (now_usec() >= deadline)
                result = -ETIMEDOUT;
            else
                result = -EAGAIN;
        }
        free(state);
        sd_bus_error_free(&error);
        sd_bus_message_unref(reply);
        if (result != -EAGAIN)
            return result;
        struct timespec delay = { .tv_sec = 0, .tv_nsec = 25000000 };
        while (nanosleep(&delay, &delay) < 0 && errno == EINTR)
            ;
    }
}

static int append_service_properties(sd_bus_message *message, const char *unit,
                                     const char *slice, const char *working_directory,
                                     char **properties, size_t nproperties,
                                     char **environment, size_t nenv,
                                     char **command, size_t ncommand) {
    int result;
    result = sd_bus_message_append(message, "(sv)", "Description", "s", unit);
    if (result < 0)
        return result;
    if (slice && slice[0]) {
        result = sd_bus_message_append(message, "(sv)", "Slice", "s", slice);
        if (result < 0)
            return result;
    }
    for (size_t i = 0; i < nproperties; i++) {
        result = append_transient_property(message, properties[i]);
        if (result < 0)
            return result;
    }
    if (working_directory && working_directory[0]) {
        result = sd_bus_message_append(message, "(sv)", "WorkingDirectory", "s",
                                       working_directory);
        if (result < 0)
            return result;
    }
    if (environment && nenv > 0) {
        result = sd_bus_message_open_container(message, 'r', "sv");
        if (result < 0)
            return result;
        result = sd_bus_message_append(message, "s", "Environment");
        if (result < 0)
            return result;
        result = sd_bus_message_open_container(message, 'v', "as");
        if (result < 0)
            return result;
        result = append_string_array(message, environment, nenv);
        if (result < 0)
            return result;
        result = sd_bus_message_close_container(message);
        if (result < 0)
            return result;
        result = sd_bus_message_close_container(message);
        if (result < 0)
            return result;
    }
    result = sd_bus_message_open_container(message, 'r', "sv");
    if (result < 0)
        return result;
    result = sd_bus_message_append(message, "s", "ExecStart");
    if (result < 0)
        return result;
    result = sd_bus_message_open_container(message, 'v', "a(sasb)");
    if (result < 0)
        return result;
    result = sd_bus_message_open_container(message, 'a', "(sasb)");
    if (result < 0)
        return result;
    result = sd_bus_message_open_container(message, 'r', "sasb");
    if (result < 0)
        return result;
    result = sd_bus_message_append(message, "s", command[0]);
    if (result < 0)
        return result;
    result = append_string_array(message, command, ncommand);
    if (result < 0)
        return result;
    result = sd_bus_message_append(message, "b", 0);
    if (result < 0)
        return result;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        return result;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        return result;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        return result;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        return result;
    return 0;
}

int onekvm_start_transient_service(const char *unit, const char *slice,
                                   const char *working_directory,
                                   char **properties, size_t nproperties,
                                   char **environment, size_t nenv,
                                   char **command, size_t ncommand,
                                   const char *socket_unit, const char *listen_address) {
    if (socket_unit && (!socket_unit[0] || !listen_address || !listen_address[0]))
        return -EINVAL;
    if (!unit || !unit[0] || !command || ncommand == 0 || !command[0] || !command[0][0])
        return -EINVAL;
    sd_bus *bus = NULL;
    sd_bus_message *message = NULL, *reply = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    /* Private connection: StartTransientUnit is also called from reconcile /
     * prewarm worker threads while the main host loop owns
     * sd_bus_default_system(). Sharing that bus from another thread returns
     * ENXIO and leaves Type=notify stuck until TimeoutStartSec. */
    int result = sd_bus_open_system(&bus);
    if (result < 0)
        goto out;
    result = sd_bus_message_new_method_call(bus, &message, "org.freedesktop.systemd1",
                                            "/org/freedesktop/systemd1",
                                            "org.freedesktop.systemd1.Manager",
                                            "StartTransientUnit");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "ss", socket_unit ? socket_unit : unit, "replace");
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'a', "(sv)");
    if (result < 0)
        goto out;
    if (socket_unit) {
        /* Matching .socket/.service names select the auxiliary service.
         * Service= is not a supported transient socket D-Bus property. */
        result = sd_bus_message_append(message, "(sv)(sv)(sv)",
                                       "Description", "s", socket_unit,
                                       "Accept", "b", 0,
                                       "CollectMode", "s", "inactive-or-failed");
        if (result < 0)
            goto out;
        result = append_property_strv(message, "BindsTo", "onekvm.service");
        if (result < 0)
            goto out;
        result = append_property_strv(message, "After", "onekvm.service");
        if (result < 0)
            goto out;
        result = append_pair_array_property(message, "Listen", "Stream", listen_address);
        if (result < 0)
            goto out;
        result = sd_bus_message_close_container(message);
        if (result < 0)
            goto out;
        result = sd_bus_message_open_container(message, 'a', "(sa(sv))");
        if (result < 0)
            goto out;
        result = sd_bus_message_open_container(message, 'r', "sa(sv)");
        if (result < 0)
            goto out;
        result = sd_bus_message_append(message, "s", unit);
        if (result < 0)
            goto out;
        result = sd_bus_message_open_container(message, 'a', "(sv)");
        if (result < 0)
            goto out;
    }
    result = append_service_properties(message, unit, slice, working_directory,
                                       properties, nproperties, environment, nenv,
                                       command, ncommand);
    if (result < 0)
        goto out;
    if (socket_unit) {
        result = append_property_strv(message, "BindsTo", socket_unit);
        if (result < 0)
            goto out;
        result = sd_bus_message_close_container(message);
        if (result < 0)
            goto out;
        result = sd_bus_message_close_container(message);
        if (result < 0)
            goto out;
        result = sd_bus_message_close_container(message);
    } else {
        result = sd_bus_message_close_container(message);
        if (result < 0)
            goto out;
        result = sd_bus_message_append(message, "a(sa(sv))", 0);
    }
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, 0, &error, &reply);
    if (result >= 0)
        result = wait_for_unit_start(bus, socket_unit ? socket_unit : unit);
    else if (result == -EEXIST && !socket_unit) {
        /* A just-stopped transient unit can remain loaded until systemd's
         * garbage collector runs. StartTransientUnit still restarts that
         * existing unit before reporting EEXIST; accept it once Type=exec has
         * reached active instead of turning a successful warm restart into a
         * reconcile failure. */
        int started = wait_for_unit_start(bus, unit);
        if (started >= 0)
            result = 0;
    }
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    sd_bus_message_unref(message);
    if (bus)
        sd_bus_flush_close_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_systemd_unit_status(const char *unit) {
    if (!unit || !unit[0])
        return -EINVAL;
    sd_bus *bus = NULL;
    sd_bus_message *reply = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    const char *path = NULL;
    char *state = NULL;
    int status = 0;
    int result = sd_bus_open_system(&bus);
    if (result < 0)
        goto out;

    result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                SYSTEMD_MANAGER, "GetUnit",
                                &error, &reply, "s", unit);
    if (!systemd_error_is_missing(&error, result)) {
        if (result < 0)
            goto out;
        result = sd_bus_message_read(reply, "o", &path);
        if (result < 0)
            goto out;
        result = sd_bus_get_property_string(bus, SYSTEMD_NAME, path,
                                            SYSTEMD_UNIT, "ActiveState",
                                            &error, &state);
        if (result < 0)
            goto out;
        if (strcmp(state, "active") == 0 || strcmp(state, "reloading") == 0)
            status |= UNIT_STATUS_ACTIVE;
        if (strcmp(state, "failed") == 0)
            status |= UNIT_STATUS_FAILED;
    }
    free(state);
    state = NULL;
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    reply = NULL;

    result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                SYSTEMD_MANAGER, "GetUnitFileState",
                                &error, &reply, "s", unit);
    if (!systemd_error_is_missing(&error, result)) {
        if (result < 0)
            goto out;
        result = sd_bus_message_read(reply, "s", &path);
        if (result < 0)
            goto out;
        if (strcmp(path, "enabled") == 0 ||
            strcmp(path, "enabled-runtime") == 0 ||
            strcmp(path, "static") == 0 || strcmp(path, "indirect") == 0 ||
            strcmp(path, "generated") == 0 || strcmp(path, "alias") == 0 ||
            strcmp(path, "linked") == 0 || strcmp(path, "linked-runtime") == 0 ||
            strcmp(path, "transient") == 0)
            status |= UNIT_STATUS_ENABLED;
    }
    result = status;
out:
    free(state);
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    if (bus)
        sd_bus_flush_close_unref(bus);
    return result;
}

int onekvm_systemd_unit_has_dependency(const char *unit, const char *property,
                                       const char *dependency) {
    if (!unit || !unit[0] || !property || !property[0] ||
        !dependency || !dependency[0])
        return -EINVAL;
    sd_bus *bus = NULL;
    sd_bus_message *reply = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    const char *path = NULL;
    char **values = NULL;
    int result = sd_bus_open_system(&bus);
    if (result < 0)
        goto out;
    result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                SYSTEMD_MANAGER, "GetUnit",
                                &error, &reply, "s", unit);
    if (systemd_error_is_missing(&error, result)) {
        result = 0;
        goto out;
    }
    if (result < 0)
        goto out;
    result = sd_bus_message_read(reply, "o", &path);
    if (result < 0)
        goto out;
    result = sd_bus_get_property_strv(bus, SYSTEMD_NAME, path, SYSTEMD_UNIT,
                                      property, &error, &values);
    if (result < 0)
        goto out;
    result = 0;
    for (size_t i = 0; values && values[i]; i++) {
        if (strcmp(values[i], dependency) == 0) {
            result = 1;
            break;
        }
    }
out:
    if (values) {
        for (size_t i = 0; values[i]; i++)
            free(values[i]);
        free(values);
    }
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    if (bus)
        sd_bus_flush_close_unref(bus);
    return result;
}

static int systemd_unit_file_action(sd_bus *bus, const char *action,
                                    const char *unit) {
    sd_bus_message *message = NULL, *reply = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    const char *method = strcmp(action, "enable") == 0
                             ? "EnableUnitFiles" : "DisableUnitFiles";
    int result = sd_bus_message_new_method_call(bus, &message, SYSTEMD_NAME,
                                                SYSTEMD_PATH, SYSTEMD_MANAGER,
                                                method);
    if (result < 0)
        goto out;
    result = sd_bus_message_open_container(message, 'a', "s");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "s", unit);
    if (result < 0)
        goto out;
    result = sd_bus_message_close_container(message);
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "b", 0);
    if (result < 0)
        goto out;
    if (strcmp(action, "enable") == 0) {
        result = sd_bus_message_append(message, "b", 0);
        if (result < 0)
            goto out;
    }
    result = sd_bus_call(bus, message, 0, &error, &reply);
    if (result < 0)
        goto out;
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    reply = NULL;
    result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                SYSTEMD_MANAGER, "Reload",
                                &error, &reply, "");
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    sd_bus_message_unref(message);
    return result < 0 ? result : 0;
}

int onekvm_systemd_unit_action(const char *action, const char *unit) {
    if (!action || !unit || !unit[0])
        return -EINVAL;
    if (strcmp(action, "enable") == 0 || strcmp(action, "disable") == 0) {
        sd_bus *bus = NULL;
        int result = sd_bus_open_system(&bus);
        if (result >= 0)
            result = systemd_unit_file_action(bus, action, unit);
        if (bus)
            sd_bus_flush_close_unref(bus);
        return result;
    }

    const char *method = NULL;
    const char *signature = "ss";
    if (strcmp(action, "start") == 0)
        method = "StartUnit";
    else if (strcmp(action, "stop") == 0)
        method = "StopUnit";
    else if (strcmp(action, "restart") == 0)
        method = "RestartUnit";
    else if (strcmp(action, "reset-failed") == 0) {
        method = "ResetFailedUnit";
        signature = "s";
    } else
        return -EINVAL;

    sd_bus *bus = NULL;
    sd_bus_message *reply = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    int result = sd_bus_open_system(&bus);
    if (result < 0)
        goto out;
    if (signature[1])
        result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                    SYSTEMD_MANAGER, method,
                                    &error, &reply, signature, unit, "replace");
    else
        result = sd_bus_call_method(bus, SYSTEMD_NAME, SYSTEMD_PATH,
                                    SYSTEMD_MANAGER, method,
                                    &error, &reply, signature, unit);
    if (systemd_error_is_missing(&error, result) &&
        (strcmp(action, "stop") == 0 || strcmp(action, "reset-failed") == 0))
        result = 0;
    if (result >= 0 &&
        (strcmp(action, "start") == 0 || strcmp(action, "restart") == 0))
        result = wait_for_unit_start(bus, unit);
    else if (result >= 0 && strcmp(action, "stop") == 0)
        result = wait_for_unit_stop(bus, unit);
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    if (bus)
        sd_bus_flush_close_unref(bus);
    return result < 0 ? result : 0;
}

static int append_string_array(sd_bus_message *message, char **values, size_t count) {
    int result = sd_bus_message_open_container(message, 'a', "s");
    if (result < 0)
        return result;
    for (size_t i = 0; i < count; i++) {
        result = sd_bus_message_append(message, "s", values[i] ? values[i] : "");
        if (result < 0)
            return result;
    }
    return sd_bus_message_close_container(message);
}

static int host_apply_config(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const char *id = NULL, *settings = NULL;
    uint64_t revision = 0;
    char **keys = NULL;
    size_t nkeys = 0;
    char *err = NULL;
    int result = sd_bus_message_read(message, "sts", &id, &revision, &settings);
    if (result < 0)
        return result;
    result = read_string_array(message, &keys, &nkeys);
    if (result < 0)
        return result;
    result = onekvm_host_apply_config(userdata, id, revision, settings, keys, nkeys, &err);
    free_string_array(keys, nkeys);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int host_simple(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const char *id = NULL;
    uint64_t revision = 0;
    char *err = NULL;
    int result = sd_bus_message_read(message, "st", &id, &revision);
    if (result < 0)
        return result;
    result = onekvm_host_simple(userdata, sd_bus_message_get_member(message), id, revision, &err);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int host_reconcile_all(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const char *generation = NULL;
    char *err = NULL;
    int result = sd_bus_message_read(message, "s", &generation);
    if (result < 0)
        return result;
    result = onekvm_host_reconcile_all(userdata, generation, &err);
    /* Always mark the Type=notify unit ready after the first reconcile attempt.
     * A single extension failure (e.g. transient unit start) must not leave the
     * host activating until TimeoutStartSec and block the rest of the system. */
    int notify = sd_notify(
        0,
        result == 0 ? "READY=1\nSTATUS=Extensions reconciled"
                    : "READY=1\nSTATUS=Extensions reconcile incomplete");
    if (result != 0) {
        if (notify < 0 && err == NULL)
            err = strdup("notify systemd after extension reconcile failed");
        return fail(error, err);
    }
    free(err);
    if (notify < 0)
        return fail(error, strdup("notify systemd after extension reconcile failed"));
    return sd_bus_reply_method_return(message, "");
}

static int host_prewarm(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    char *err = NULL;
    int result = onekvm_host_prewarm(userdata, &err);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int host_invoke(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const char *id = NULL, *method = NULL, *request = NULL;
    char *response = NULL, *err = NULL;
    int result = sd_bus_message_read(message, "sss", &id, &method, &request);
    if (result < 0)
        return result;
    result = onekvm_host_invoke(userdata, id, method, request, &response, &err);
    if (result != 0) {
        free(response);
        return fail(error, err);
    }
    free(err);
    result = sd_bus_reply_method_return(message, "s", response ? response : "null");
    free(response);
    return result;
}

static int on_name_owner(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const char *name = NULL, *old_owner = NULL, *new_owner = NULL;
    (void)error;
    if (sd_bus_message_read(message, "sss", &name, &old_owner, &new_owner) < 0)
        return 0;
    if (name && strcmp(name, CORE_NAME) == 0 && (!new_owner || new_owner[0] == '\0'))
        onekvm_host_core_lost(userdata);
    return 0;
}

static const sd_bus_vtable host_vtable[] = {
    SD_BUS_VTABLE_START(0),
    SD_BUS_METHOD("ApplyConfig", "stsas", "", host_apply_config, 0),
    SD_BUS_METHOD("CaptureTransaction", "st", "", host_simple, 0),
    SD_BUS_METHOD("RestoreTransaction", "st", "", host_simple, 0),
    SD_BUS_METHOD("CheckHealth", "st", "", host_simple, 0),
    SD_BUS_METHOD("Enable", "st", "", host_simple, 0),
    SD_BUS_METHOD("Disable", "st", "", host_simple, 0),
    SD_BUS_METHOD("Reconcile", "st", "", host_simple, 0),
    SD_BUS_METHOD("LoadNetworkModule", "st", "", host_simple, 0),
    SD_BUS_METHOD("Prewarm", "", "", host_prewarm, 0),
    SD_BUS_METHOD("ReconcileAll", "s", "", host_reconcile_all, 0),
    SD_BUS_METHOD("Invoke", "sss", "s", host_invoke, 0),
    SD_BUS_VTABLE_END,
};

int onekvm_host_serve(void *host) {
    sd_bus *bus = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    result = sd_bus_add_object_vtable(bus, NULL, HOST_PATH, HOST_IFACE, host_vtable, host);
    if (result < 0)
        goto out;
    result = sd_bus_add_match(bus, NULL,
                              "type='signal',sender='org.freedesktop.DBus',"
                              "path='/org/freedesktop/DBus',"
                              "interface='org.freedesktop.DBus',"
                              "member='NameOwnerChanged',arg0='" CORE_NAME "'",
                              on_name_owner, host);
    if (result < 0)
        goto out;
    result = sd_bus_request_name(bus, HOST_NAME, 0);
    if (result < 0)
        goto out;
    for (;;) {
        result = sd_bus_process(bus, NULL);
        if (result < 0)
            goto out;
        if (result > 0)
            continue;
        result = sd_bus_wait(bus, UINT64_MAX);
        if (result < 0)
            goto out;
    }
out:
    sd_bus_unref(bus);
    return result;
}

static int helper_ping(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    char *id = NULL, *err = NULL;
    int result = onekvm_helper_ping(userdata, &id, &err);
    if (result != 0) {
        free(id);
        return fail(error, err);
    }
    free(err);
    result = sd_bus_reply_method_return(message, "s", id ? id : "");
    free(id);
    return result;
}

static int helper_run_controller(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const void *payload = NULL;
    size_t size = 0;
    void *output = NULL;
    size_t out_size = 0;
    char *err = NULL;
    int result = sd_bus_message_read_array(message, 'y', &payload, &size);
    if (result < 0)
        return result;
    result = onekvm_helper_run_controller(userdata, payload, size, &output, &out_size, &err);
    if (result != 0) {
        free(output);
        return fail(error, err);
    }
    free(err);
    sd_bus_message *reply = NULL;
    result = sd_bus_message_new_method_return(message, &reply);
    if (result < 0) {
        free(output);
        return result;
    }
    result = sd_bus_message_append_array(reply, 'y', output, out_size);
    free(output);
    if (result < 0) {
        sd_bus_message_unref(reply);
        return result;
    }
    result = sd_bus_send(NULL, reply, NULL);
    sd_bus_message_unref(reply);
    return result;
}

static int helper_ensure(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    const char *name = NULL, *restart = NULL;
    char **command = NULL, **environment = NULL;
    size_t ncommand = 0, nenv = 0;
    char *err = NULL;
    int result = sd_bus_message_read(message, "s", &name);
    if (result < 0)
        return result;
    result = read_string_array(message, &command, &ncommand);
    if (result < 0)
        return result;
    result = read_string_array(message, &environment, &nenv);
    if (result < 0) {
        free_string_array(command, ncommand);
        return result;
    }
    result = sd_bus_message_read(message, "s", &restart);
    if (result < 0) {
        free_string_array(command, ncommand);
        free_string_array(environment, nenv);
        return result;
    }
    result = onekvm_helper_ensure(userdata, name, command, ncommand, environment, nenv,
                                  restart, &err);
    free_string_array(command, ncommand);
    free_string_array(environment, nenv);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int helper_named(sd_bus_message *message, void *userdata, sd_bus_error *error,
                        int (*fn)(void *, const char *, char **)) {
    const char *name = NULL;
    char *err = NULL;
    int result = sd_bus_message_read(message, "s", &name);
    if (result < 0)
        return result;
    result = fn(userdata, name, &err);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int helper_remove(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    return helper_named(message, userdata, error, onekvm_helper_remove);
}

static int helper_restart(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    return helper_named(message, userdata, error, onekvm_helper_restart);
}

static int helper_remove_all(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    char *err = NULL;
    int result = onekvm_helper_remove_all(userdata, &err);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int helper_exec(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    char **command = NULL, **environment = NULL;
    size_t ncommand = 0, nenv = 0;
    uint32_t timeout = 0;
    char *err = NULL;
    int result = read_string_array(message, &command, &ncommand);
    if (result < 0)
        return result;
    result = read_string_array(message, &environment, &nenv);
    if (result < 0) {
        free_string_array(command, ncommand);
        return result;
    }
    result = sd_bus_message_read(message, "u", &timeout);
    if (result < 0) {
        free_string_array(command, ncommand);
        free_string_array(environment, nenv);
        return result;
    }
    result = onekvm_helper_exec(userdata, command, ncommand, environment, nenv, timeout, &err);
    free_string_array(command, ncommand);
    free_string_array(environment, nenv);
    if (result != 0)
        return fail(error, err);
    free(err);
    return sd_bus_reply_method_return(message, "");
}

static int helper_exec_capture(sd_bus_message *message, void *userdata,
                               sd_bus_error *error) {
    char **command = NULL, **environment = NULL;
    size_t ncommand = 0, nenv = 0, size = 0, out_size = 0;
    const void *payload = NULL;
    void *output = NULL;
    sd_bus_message *reply = NULL;
    uint32_t timeout = 0;
    char *err = NULL;
    int result = read_string_array(message, &command, &ncommand);
    if (result < 0)
        return result;
    result = read_string_array(message, &environment, &nenv);
    if (result < 0)
        goto out;
    result = sd_bus_message_read_array(message, 'y', &payload, &size);
    if (result < 0)
        goto out;
    result = sd_bus_message_read(message, "u", &timeout);
    if (result < 0)
        goto out;
    result = onekvm_helper_exec_capture(userdata, command, ncommand,
                                        environment, nenv, payload, size,
                                        timeout, &output, &out_size, &err);
    if (result != 0) {
        result = fail(error, err);
        err = NULL;
        goto out;
    }
    result = sd_bus_message_new_method_return(message, &reply);
    if (result >= 0)
        result = sd_bus_message_append_array(reply, 'y', output, out_size);
    if (result >= 0)
        result = sd_bus_send(NULL, reply, NULL);
    sd_bus_message_unref(reply);
out:
    free(err);
    free(output);
    free_string_array(command, ncommand);
    free_string_array(environment, nenv);
    return result;
}

static int helper_instances(sd_bus_message *message, void *userdata, sd_bus_error *error) {
    char **names = NULL;
    size_t count = 0;
    char *err = NULL;
    int result = onekvm_helper_instances(userdata, &names, &count, &err);
    if (result != 0)
        return fail(error, err);
    free(err);
    sd_bus_message *reply = NULL;
    result = sd_bus_message_new_method_return(message, &reply);
    if (result < 0) {
        free_string_array(names, count);
        return result;
    }
    result = append_string_array(reply, names, count);
    free_string_array(names, count);
    if (result < 0) {
        sd_bus_message_unref(reply);
        return result;
    }
    result = sd_bus_send(NULL, reply, NULL);
    sd_bus_message_unref(reply);
    return result;
}

static const sd_bus_vtable helper_vtable[] = {
    SD_BUS_VTABLE_START(0),
    SD_BUS_METHOD("Ping", "", "s", helper_ping, 0),
    SD_BUS_METHOD("RunController", "ay", "ay", helper_run_controller, 0),
    SD_BUS_METHOD("Ensure", "sasass", "", helper_ensure, 0),
    SD_BUS_METHOD("Remove", "s", "", helper_remove, 0),
    SD_BUS_METHOD("RemoveAll", "", "", helper_remove_all, 0),
    SD_BUS_METHOD("Restart", "s", "", helper_restart, 0),
    SD_BUS_METHOD("Exec", "asasu", "", helper_exec, 0),
    SD_BUS_METHOD("ExecCapture", "asasayu", "ay", helper_exec_capture, 0),
    SD_BUS_METHOD("Instances", "", "as", helper_instances, 0),
    SD_BUS_VTABLE_END,
};

static int helper_client_bus(const char *unique, sd_bus **bus, sd_bus_message **reply,
                             const char *method, const char *types, ...) {
    sd_bus *connection = NULL;
    int result = sd_bus_default_system(&connection);
    if (result < 0)
        return result;
    sd_bus_message *message = NULL;
    result = sd_bus_message_new_method_call(connection, &message, unique, HELPER_PATH, HELPER_IFACE,
                                            method);
    if (result < 0)
        goto out;
    if (types && types[0]) {
        va_list args;
        va_start(args, types);
        result = sd_bus_message_appendv(message, types, args);
        va_end(args);
        if (result < 0)
            goto out;
    }
    result = sd_bus_call(connection, message, 35000000, NULL, reply);
out:
    sd_bus_message_unref(message);
    if (result < 0)
        sd_bus_unref(connection);
    else
        *bus = connection;
    return result;
}

int onekvm_helper_client_ping(const char *unique, char **id) {
    sd_bus *bus = NULL;
    sd_bus_message *reply = NULL;
    int result = helper_client_bus(unique, &bus, &reply, "Ping", NULL);
    if (result < 0)
        return result;
    const char *value = NULL;
    result = sd_bus_message_read(reply, "s", &value);
    if (result >= 0)
        *id = strdup(value ? value : "");
    sd_bus_message_unref(reply);
    sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_helper_client_run_controller(const char *unique, const void *payload, size_t size,
                                        void **output, size_t *out_size, char **err) {
    sd_bus *bus = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    sd_bus_message *message = NULL, *reply = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    result = sd_bus_message_new_method_call(bus, &message, unique, HELPER_PATH, HELPER_IFACE,
                                            "RunController");
    if (result < 0)
        goto out;
    result = sd_bus_message_append_array(message, 'y', payload, size);
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, 35000000, &error, &reply);
    if (result < 0)
        goto out;
    const void *data = NULL;
    size_t nbytes = 0;
    result = sd_bus_message_read_array(reply, 'y', &data, &nbytes);
    if (result >= 0) {
        void *copy = malloc(nbytes ? nbytes : 1);
        if (!copy)
            result = -ENOMEM;
        else {
            memcpy(copy, data, nbytes);
            *output = copy;
            *out_size = nbytes;
            result = 0;
        }
    }
out:
    if (result < 0 && err)
        *err = strdup(sd_bus_error_is_set(&error) ? error.message : strerror(-result));
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result;
}

int onekvm_helper_client_ensure(const char *unique, const char *name, char **command, size_t ncommand,
                                char **environment, size_t nenv, const char *restart) {
    sd_bus *bus = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    sd_bus_message *message = NULL;
    result = sd_bus_message_new_method_call(bus, &message, unique, HELPER_PATH, HELPER_IFACE, "Ensure");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "s", name);
    if (result < 0)
        goto out;
    result = append_string_array(message, command, ncommand);
    if (result < 0)
        goto out;
    result = append_string_array(message, environment, nenv);
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "s", restart);
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, 35000000, NULL, NULL);
out:
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_helper_client_simple(const char *unique, const char *method, const char *name) {
    sd_bus *bus = NULL;
    sd_bus_message *reply = NULL;
    int result;
    if (name)
        result = helper_client_bus(unique, &bus, &reply, method, "s", name);
    else
        result = helper_client_bus(unique, &bus, &reply, method, NULL);
    sd_bus_message_unref(reply);
    if (bus)
        sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_helper_client_exec(const char *unique, char **command, size_t ncommand, char **environment,
                              size_t nenv, uint32_t timeout) {
    sd_bus *bus = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    sd_bus_message *message = NULL;
    result = sd_bus_message_new_method_call(bus, &message, unique, HELPER_PATH, HELPER_IFACE, "Exec");
    if (result < 0)
        goto out;
    result = append_string_array(message, command, ncommand);
    if (result < 0)
        goto out;
    result = append_string_array(message, environment, nenv);
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "u", timeout);
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, (timeout + 2) * 1000000ull, NULL, NULL);
out:
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_helper_client_exec_capture(const char *unique, char **command,
                                      size_t ncommand, char **environment,
                                      size_t nenv, const void *payload,
                                      size_t size, uint32_t timeout,
                                      void **output, size_t *out_size, char **err) {
    sd_bus *bus = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    sd_bus_message *message = NULL, *reply = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    result = sd_bus_message_new_method_call(bus, &message, unique, HELPER_PATH,
                                            HELPER_IFACE, "ExecCapture");
    if (result < 0)
        goto out;
    result = append_string_array(message, command, ncommand);
    if (result < 0)
        goto out;
    result = append_string_array(message, environment, nenv);
    if (result < 0)
        goto out;
    result = sd_bus_message_append_array(message, 'y', payload, size);
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "u", timeout);
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, (timeout + 2) * 1000000ull, &error, &reply);
    if (result < 0)
        goto out;
    const void *data = NULL;
    size_t nbytes = 0;
    result = sd_bus_message_read_array(reply, 'y', &data, &nbytes);
    if (result >= 0) {
        void *copy = malloc(nbytes ? nbytes : 1);
        if (!copy)
            result = -ENOMEM;
        else {
            memcpy(copy, data, nbytes);
            *output = copy;
            *out_size = nbytes;
            result = 0;
        }
    }
out:
    if (result < 0 && err)
        *err = strdup(sd_bus_error_is_set(&error) ? error.message : strerror(-result));
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_helper_serve(void *helper) {
    sd_bus *bus = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    result = sd_bus_add_object_vtable(bus, NULL, HELPER_PATH, HELPER_IFACE, helper_vtable, helper);
    if (result < 0)
        goto out;
    const char *unique = NULL;
    result = sd_bus_get_unique_name(bus, &unique);
    if (result < 0)
        goto out;
    result = onekvm_helper_set_address(helper, unique);
    if (result != 0) {
        result = -EIO;
        goto out;
    }
    for (;;) {
        result = sd_bus_process(bus, NULL);
        if (result < 0)
            goto out;
        if (result > 0)
            continue;
        if (onekvm_helper_should_exit(helper))
            break;
        result = sd_bus_wait(bus, 1000000ull);
        if (result < 0)
            goto out;
        if (onekvm_helper_should_exit(helper))
            break;
    }
    result = 0;
out:
    sd_bus_unref(bus);
    return result;
}

static int cli_set_error(char **err, const char *text) {
    if (!err)
        return -EIO;
    *err = strdup(text && text[0] ? text : "extension host call failed");
    return *err ? -EIO : -ENOMEM;
}

int onekvm_cli_host_simple(const char *method, const char *id, uint64_t revision, char **err) {
    sd_bus *bus = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    sd_bus_message *message = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return cli_set_error(err, strerror(-result));
    result = sd_bus_message_new_method_call(bus, &message, HOST_NAME, HOST_PATH, HOST_IFACE, method);
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "st", id, revision);
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, 35000000, &error, NULL);
    if (result < 0)
        result = cli_set_error(err, sd_bus_error_is_set(&error) ? error.message : strerror(-result));
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_cli_host_invoke(const char *id, const char *method, const char *request, char **response,
                           char **err) {
    sd_bus *bus = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    sd_bus_message *message = NULL;
    sd_bus_message *reply = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return cli_set_error(err, strerror(-result));
    result = sd_bus_message_new_method_call(bus, &message, HOST_NAME, HOST_PATH, HOST_IFACE, "Invoke");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "sss", id, method, request ? request : "null");
    if (result < 0)
        goto out;
    /* Auth confirm methods wait up to 120s; keep a small margin over that. */
    result = sd_bus_call(bus, message, 130000000, &error, &reply);
    if (result < 0) {
        result = cli_set_error(err, sd_bus_error_is_set(&error) ? error.message : strerror(-result));
        goto out;
    }
    const char *value = NULL;
    result = sd_bus_message_read(reply, "s", &value);
    if (result < 0)
        goto out;
    *response = strdup(value ? value : "null");
    result = *response ? 0 : -ENOMEM;
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result;
}

int onekvm_cli_host_apply_config(const char *id, uint64_t revision, const char *settings, char **keys,
                                 size_t nkeys, char **err) {
    sd_bus *bus = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    sd_bus_message *message = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return cli_set_error(err, strerror(-result));
    result = sd_bus_message_new_method_call(bus, &message, HOST_NAME, HOST_PATH, HOST_IFACE,
                                            "ApplyConfig");
    if (result < 0)
        goto out;
    result = sd_bus_message_append(message, "sts", id, revision, settings ? settings : "{}");
    if (result < 0)
        goto out;
    result = append_string_array(message, keys, nkeys);
    if (result < 0)
        goto out;
    result = sd_bus_call(bus, message, 35000000, &error, NULL);
    if (result < 0)
        result = cli_set_error(err, sd_bus_error_is_set(&error) ? error.message : strerror(-result));
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result < 0 ? result : 0;
}

int onekvm_cli_dbus_call_s(const char *dest, const char *path, const char *iface, const char *method,
                           const char *arg, int has_arg, char **response, int want_response,
                           char **err) {
    sd_bus *bus = NULL;
    sd_bus_error error = SD_BUS_ERROR_NULL;
    sd_bus_message *message = NULL;
    sd_bus_message *reply = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return cli_set_error(err, strerror(-result));
    result = sd_bus_message_new_method_call(bus, &message, dest, path, iface, method);
    if (result < 0)
        goto out;
    if (has_arg) {
        result = sd_bus_message_append(message, "s", arg ? arg : "");
        if (result < 0)
            goto out;
    }
    result = sd_bus_call(bus, message, 5000000, &error, want_response ? &reply : NULL);
    if (result < 0) {
        result = cli_set_error(err, sd_bus_error_is_set(&error) ? error.message : strerror(-result));
        goto out;
    }
    /* sd_bus_call returns a positive byte count on success. Callers treat any
     * non-zero FFI status as failure, so map success to 0. */
    result = 0;
    if (want_response && response) {
        const char *value = NULL;
        result = sd_bus_message_read(reply, "s", &value);
        if (result < 0)
            goto out;
        *response = strdup(value ? value : "");
        result = *response ? 0 : -ENOMEM;
    }
out:
    sd_bus_error_free(&error);
    sd_bus_message_unref(reply);
    sd_bus_message_unref(message);
    sd_bus_unref(bus);
    return result;
}
