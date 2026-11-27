#include <errno.h>
#include <fcntl.h>
#include <linux/loop.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <unistd.h>

/* LOOP_CONFIGURE binds the exact already authenticated file description.
 * AUTOCLEAR releases it on the last reference, including when mount fails. */
int onekvm_mount_runtime_image(int image, const char *target) {
    int control = open("/dev/loop-control", O_RDWR | O_CLOEXEC);
    if (control < 0)
        return -errno;

    struct loop_config config = {
        .fd = image,
        .info.lo_flags = LO_FLAGS_AUTOCLEAR | LO_FLAGS_READ_ONLY,
    };
    int result = -EBUSY;
    for (unsigned attempt = 0; attempt < 16; attempt++) {
        int number = ioctl(control, LOOP_CTL_GET_FREE);
        if (number < 0) {
            result = -errno;
            break;
        }
        char path[32];
        snprintf(path, sizeof(path), "/dev/loop%d", number);
        int loop = open(path, O_RDWR | O_CLOEXEC | O_NOFOLLOW);
        if (loop < 0) {
            result = -errno;
            break;
        }
        if (ioctl(loop, LOOP_CONFIGURE, &config) < 0) {
            result = -errno;
            close(loop);
            if (result == -EBUSY)
                continue;
            break;
        }
        result = mount(path, target, "squashfs", MS_RDONLY | MS_NODEV | MS_NOSUID, NULL);
        if (result < 0)
            result = -errno;
        close(loop);
        break;
    }
    close(control);
    return result;
}
