/* Copyright (c) 2026 Henrique Falconer. All rights reserved. */
/* SPDX-License-Identifier: Proprietary */
#ifndef BAUD_TAPE_H
#define BAUD_TAPE_H

#include <sys/ioctl.h>

#define BAUD_TAPE_CONTROL_TYPE 'B'
#define BAUD_TAPE_PROBE _IO(BAUD_TAPE_CONTROL_TYPE, 0)
#define BAUD_TAPE_MARK_BRANCH _IO(BAUD_TAPE_CONTROL_TYPE, 1)
#define BAUD_TAPE_GOAL _IO(BAUD_TAPE_CONTROL_TYPE, 2)
#define BAUD_TAPE_VIOLATION _IO(BAUD_TAPE_CONTROL_TYPE, 3)
#define BAUD_TAPE_LOG _IO(BAUD_TAPE_CONTROL_TYPE, 4)
#define BAUD_TAPE_FRAME _IO(BAUD_TAPE_CONTROL_TYPE, 5)

#endif
