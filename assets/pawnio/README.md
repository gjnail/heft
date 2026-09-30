# PawnIO modules

These are unmodified, signed binaries from the official PawnIO.Modules
release [0.2.11](https://github.com/namazso/PawnIO.Modules/releases/tag/0.2.11)
by namazso. Heft embeds them and loads them into the
[PawnIO](https://pawnio.eu) driver, but only when the user has installed
PawnIO themselves and runs Heft as administrator. Heft never installs the
driver.

| File | Used for | SHA-256 |
|---|---|---|
| `AMDFamily17.bin` | AMD Zen (family 17h to 1Ah) temperature (SMN) and package energy (MSR) | `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91` |
| `IntelMSR.bin` | Intel core and package temperature and RAPL energy (MSR) | `d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f` |
| `LpcIO.bin` | The motherboard's Super I/O sensor chip (fans, voltages, temperatures) | `b3896a1cab0d808fca31fe2ebcae045d59dac690da87b17c858bb8da357eb45e` |

The driver checks each module's signature, and each module only allows the
registers and I/O ports its hardware needs.

## License

The modules are licensed under the GNU Lesser General Public License,
version 2.1 or later; the full text is in [COPYING](COPYING). Their source
code is at <https://github.com/namazso/PawnIO.Modules> (`AMDFamily17.p`,
`IntelMSR.p`, `LpcIO.p`).

To use different builds of these modules, replace the files here and rebuild
Heft (`src/sensors/win/pawnio.rs` includes them with `include_bytes!`). The
driver only loads modules signed by the PawnIO project.

## Updating

Download the release zip from the link above, check the files against the
release, copy the three `.bin` files here, and update the version and
hashes in this file.
