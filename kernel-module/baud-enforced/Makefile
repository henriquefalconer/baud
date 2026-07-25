# SPDX-License-Identifier: GPL-2.0
#
# Builds baud_enforced_probe.ko against the currently running kernel. Needs
# /lib/modules/$(uname -r)/build to point at a prepared kernel source tree
# (`make modules_prepare`) — on this WSL2 dev machine that tree is the
# microsoft/WSL2-Linux-Kernel checkout at the exact running kernel's tag, since
# the stock WSL2 kernel ships no linux-headers-* package (see CLAUDE.md).

obj-m += baud_enforced_probe.o

KDIR ?= /lib/modules/$(shell uname -r)/build
PWD := $(shell pwd)

.PHONY: all clean

all:
	$(MAKE) -C $(KDIR) M=$(PWD) modules

clean:
	$(MAKE) -C $(KDIR) M=$(PWD) clean
