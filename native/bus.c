#define _GNU_SOURCE
#include <errno.h>
#include <stdarg.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <systemd/sd-bus.h>
#include <time.h>

#define HOST_NAME "org.onekvm.ExtensionHost1"
#define HOST_PATH "/org/onekvm/ExtensionHost1"
#define HOST_IFACE "org.onekvm.ExtensionHost1"
#define HOST_ERR "org.onekvm.ExtensionHost1.Error.Failed"
#define HELPER_PATH "/org/onekvm/ExtensionHelper1"
#define HELPER_IFACE "org.onekvm.ExtensionHelper1"
#define CORE_NAME "org.onekvm.Core1"
#define IDLE_USEC (30 * 1000000ull)

int onekvm_host_apply_config(void *host, const char *id, uint64_t revision,
                             const char *settings, char **keys, size_t nkeys,
                             char **err);
int onekvm_host_simple(void *host, const char *method, const char *id,
                       uint64_t revision, char **err);
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
    SD_BUS_METHOD("Enable", "st", "", host_simple, 0),
    SD_BUS_METHOD("Disable", "st", "", host_simple, 0),
    SD_BUS_METHOD("Reconcile", "st", "", host_simple, 0),
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
    uint64_t deadline = now_usec() + IDLE_USEC;
    for (;;) {
        result = sd_bus_process(bus, NULL);
        if (result < 0)
            goto out;
        if (result > 0) {
            deadline = now_usec() + IDLE_USEC;
            continue;
        }
        uint64_t now = now_usec();
        if (now >= deadline)
            break;
        result = sd_bus_wait(bus, deadline - now);
        if (result < 0)
            goto out;
    }
    result = 0;
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
                                        void **output, size_t *out_size) {
    sd_bus *bus = NULL;
    int result = sd_bus_default_system(&bus);
    if (result < 0)
        return result;
    sd_bus_message *message = NULL;
    result = sd_bus_message_new_method_call(bus, &message, unique, HELPER_PATH, HELPER_IFACE,
                                            "RunController");
    if (result < 0)
        goto out;
    result = sd_bus_message_append_array(message, 'y', payload, size);
    if (result < 0)
        goto out;
    sd_bus_message *reply = NULL;
    result = sd_bus_call(bus, message, 35000000, NULL, &reply);
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
    sd_bus_message_unref(reply);
out:
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
    return result;
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
    return result;
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
