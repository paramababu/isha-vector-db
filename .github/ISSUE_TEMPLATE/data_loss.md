---
name: Data loss or corruption
about: A database will not open, or data went missing
labels: bug, data-loss, priority
---

**Please do not delete the affected database.** A copy of it, or a description of its structure,
is usually what makes the difference between a fix and a guess.

**What happened**

**Sequence of events before it**

<!-- Was the app killed? Did the device lose power or run out of space? Was a migration running?
     Was the same database open in two processes? -->

**Error output**

**Environment**
- vdb version / commit:
- Platform, OS version, device:
- Storage backend and durability mode:
- Approximate collection size:

**Diagnostics**

<!-- If you can run it: `isha-vector-db verify --full <path>` and `isha-vector-db inspect <path>`. -->
