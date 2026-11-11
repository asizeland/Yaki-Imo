Proposed future items to "flesh out" Yakimo for developers:

Currently, I believe the main hurdles for developers utilizing Yakimo (or the eventual distribution built on Yakimo) are as follows:


## Interactive debug shell with incremental builds.
I believe one of the first items which should be addressed is the lack of a dedicated debugging space for developers/people working on packages. Where through a debugging shell consisting of a interactive shell inside a copy of the environment used by a package build. 
```
pkg debug-shell foobar
```
- This sould provide the same declared dependencies, environment variables, paths and sanbox restriction.
- Access to build log and generated configuration.
- A way to run commands, inspect files and test proposed fixes.
- No network access by default, matching ordinary build.
- No permission to modify the live installed root.
- Incremental builds only within this interactive shell (as to avoid damaging reproducibility), allowing the user to:
```
pkg debug-shell --resume foobar
```
Alongside with a manual method of clearing the build tree. 
```
pkg debug-shell --remove-tree
```
Furthermore, this could be further improved by allowing:
```
pkg debug-shell --phase prepare foobar
pkg debug-shell --phase compile foobar
pkg debug-shell --phase install foobar
```
All of this sould allow the user to quickly reach the part of their build which failed, and make changes to the recipe or other files relevant to the build. These changes should be temporary, prior to exit from shell, Yakimo should offer to generate a patch (e.g. foobar-patch package recipe). On exit from this shell the build tree should be discarded entirely to not damage reproducibility.


## ABI checking

A maintainer tool designed to check old and new libraries for removed or added symbols, changed symbol versions, changed function signatures where debug information permits, structure size and layout changes, soname changes, new or removed ELF dependencies. Via the command:
```
pkg abi-diff libfoobar-2.1 libfoobar-2.0
#or 
pkg abi-diff libfoobar-2.0 libfoobar-2.1
```
The report should feed into maintainer review and rebuild planning. It should not assume that every added symbol requires a rebuild or that unchanged symbols prove compatibility. Some ABI changes are invisible to simple ELF inspection. Tests and upstream knowledge remain necessary.

Furthermore, there is currently a lack of hooks

## Transaction hook expansion.

The current build system could probably benefit from a declarative hook system, for actions such as rebuilding font caches, updating MIME data, registering services, creating groups and registering groups and rebuilding stuff like initramfs.

- These should run after activation, their inputs and order should be deterministic
- They run with restricted permissions and explicit filesystem access
- Their output and failiures logged (ensure human readable format)
- Failed hook means a failed transaction (to avoid a failed hook leave the database with unfinished operations)


## Kernel packages

In my experience different user have different preferences for the exact kernel they are using (regardless of benefits), therefore I would like a few versions of the kernel packaged. LTS, latest, RC (release candidate), Hardened kernel and CachyOS kernel.

Furthermore, regarding this, I wonder if it would be possible in Linux to implement something similar to how FreeBSD allows you to boot several different kernels, I understand it is different in nature as a monolithic project. However perhaps something similar is possible via the grub2 or Limine bootloaders.

## Package recipe naming convention. 

Considering the previously mentioned patch system from the debug shell, it would be very convenient to change the naming conventions of the actual files to something along the lines:
```
foobar.pkg
foobar.watch
```
This way any patched files can simply appear as:
```
foobar-patch.pkg
foobar-patch.watch
```
