-- Copyright (c) 2026 Henrique Falconer. All rights reserved.
-- SPDX-License-Identifier: Proprietary

-- Live frame appenders may provide the validated pixel payload. Keeping it with the frame record
-- makes SSE tail a real stream instead of a hash-only polling endpoint. KVM fuzz persistence still
-- omits this column and regenerates pixels from the recorded run when rendering.
ALTER TABLE frame_records ADD COLUMN pixels BLOB;
