<!--
SPDX-FileCopyrightText: 2026 roolrz
SPDX-License-Identifier: Apache-2.0
-->

# lib-vm-support

Reusable VM device and I/O appliance mechanisms.

Provides block/I/O wire protocols, PCI/PCIe helpers, virtio models, queues, console and
network-device mechanisms. VM services select and own these mechanisms for each guest.

See the [implementation entry](src/lib.rs) and [component declaration](component.json)
for the public interfaces, build dependencies and delivered artifacts.

## Boundaries

Process supervision, console-session policy and crash fixtures belong to consumers.
Virtual-device networking here does not imply a Native userspace network protocol stack.

## Build and test

Use this checkout inside the [repo-managed workspace](https://github.com/HypeR-Proj/HypeR).
From this repository directory:

```sh
make
make test
```

An absolute `OUT=/path/to/out` selects a shared output directory. The module
entry delegates to build/system, which prepares the installed SDK and declared
dependencies. Native binaries require HypeR; host module checks cover the tests
selected by component.json. A standalone clone does not contain the workspace’s
other source repositories or Native SDK.
