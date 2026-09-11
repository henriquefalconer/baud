// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary
//
// Minimal guest endpoint for the baud tape PIO device. The host owns the protocol and
// determinism rules. This driver only provides blocking byte reads and writes through
// /dev/baud-tape, so a workload cannot accidentally read a host file or entropy source.

#include <linux/fs.h>
#include <linux/init.h>
#include <linux/io.h>
#include <linux/miscdevice.h>
#include <linux/module.h>
#include <linux/poll.h>
#include <linux/uaccess.h>

#define BAUD_TAPE_BASE 0x0500
#define BAUD_TAPE_DATA 0x00
#define BAUD_TAPE_CONTROL 0x08
#define BAUD_TAPE_STATUS 0x10

static ssize_t baud_tape_read(struct file *file, char __user *out, size_t len,
                              loff_t *offset)
{
    size_t i;
    u8 byte;

    if (len == 0)
        return 0;
    for (i = 0; i < len; i++) {
        byte = inb(BAUD_TAPE_BASE + BAUD_TAPE_DATA);
        if (copy_to_user(out + i, &byte, 1))
            return i ? (ssize_t)i : -EFAULT;
    }
    return (ssize_t)i;
}

static ssize_t baud_tape_write(struct file *file, const char __user *in, size_t len,
                               loff_t *offset)
{
    size_t i;
    u8 byte;

    for (i = 0; i < len; i++) {
        if (copy_from_user(&byte, in + i, 1))
            return i ? (ssize_t)i : -EFAULT;
        outb(byte, BAUD_TAPE_BASE + BAUD_TAPE_DATA);
    }
    return (ssize_t)i;
}

static long baud_tape_ioctl(struct file *file, unsigned int command,
                            unsigned long argument)
{
    u8 opcode = (u8)command;

    /* The control opcode is deliberately an ioctl number, not data in the stream.
     * This lets a harness write an opaque payload and commit it atomically. */
    if (_IOC_TYPE(command) != 'B')
        return -ENOTTY;
    opcode = (u8)_IOC_NR(command);
    if (opcode > 5)
        return -EINVAL;
    outb(opcode, BAUD_TAPE_BASE + BAUD_TAPE_CONTROL);
    return 0;
}

static __poll_t baud_tape_poll(struct file *file, poll_table *wait)
{
    /* The host's tape is finite and status is deterministic. Reads never block on host I/O. */
    return POLLIN | POLLOUT;
}

static const struct file_operations baud_tape_fops = {
    .owner = THIS_MODULE,
    .read = baud_tape_read,
    .write = baud_tape_write,
    .unlocked_ioctl = baud_tape_ioctl,
    .poll = baud_tape_poll,
    .llseek = noop_llseek,
};

static struct miscdevice baud_tape_device = {
    .minor = MISC_DYNAMIC_MINOR,
    .name = "tape",
    .fops = &baud_tape_fops,
    .mode = 0600,
};

static int __init baud_tape_init(void)
{
    return misc_register(&baud_tape_device);
}
subsys_initcall(baud_tape_init);

static void __exit baud_tape_exit(void)
{
    misc_deregister(&baud_tape_device);
}
module_exit(baud_tape_exit);

MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("Deterministic baud tape endpoint");
MODULE_AUTHOR("baud");
