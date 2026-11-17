# FEx
FEx (File EXplorer or File Explorer Xtreme, depending on your mood) is a cross-platform filesystem browser primarily intended to offer a more expedient experience for finding files on your computer and network drives.

FEx runs a background indexing service to build an in-memory table of file indices, which the frontend then queries. This allows FEx to return search results much faster than the built-in Windows file explorer. 

The indexing service is configured to use system resources greedily *only when your CPU is idle*, which lets FEx give rapid responses without impairing the functionality of other programs.

Currently, FEx has builds for both Windows and Linux, and while I am working to have both clients enjoy feature parity, there may be deltas between the two. 

## Installing on Windows
Download ```fex-[TARGET VERSION]-windows-x64.zip```

From an elevated command prompt in the extracted folder, run:

```./fex service install```

You can dynamically tune resource consumption based on system hardware with ```./fex service profile```

The FEx exe file can then be pinned to your taskbar or launched from any folder.

Starting with v0.1.0, you can use ```./fex update check``` and ```./fex update``` to update the frontend and indexing service without rebuilding index memory.


## Installing on Linux

``` cd  [TARGET FOLDER] ```

``` curl -fLO https://github.com/DMartinCodes/FEx/releases/download/v[TARGET VERSION]/fex-[TARGET VERSION]-linux-x64.tar.gz```

As a note, I am currently only targeting x86_64 architecture, but may target ARM in the future.

``` tar xzf fex-[TARGET VERSION]-linux-x64.tar.gz && cd fex-[TARGET VERSION] ```

The indexing service is root-owned so you will need admin privileges to install.

``` sudo install -o root -g root -m755 fex /usr/local/bin/fex ```

``` sudo install -m644 fex-indexd.service /etc/systemd/system/ ```

The indexing daemon will profile your hardware to identify resource consumption limits. 

``` sudo ./fex profile --save ``` 

``` sudo systemctl daemon-reload && sudo systemctl enable --now fex-indexd ```

You can then run FEx with ```fex``` . The GUI is a work in progress, but there is a CLI interaction method as well: ```fex query --scope [FOLDER] [KEYWORD]```

FEx has update functionality where it can pull the latest release from this repo (the index does not have to be rebuilt). You can run ```fex update check``` to see if there's a newer version, and ```sudo fex update``` will update the tool.

```fex status``` shows some handy info about your system and how the service is consuming resources.


*Fair warning, the Linux build may not always have full feature parity with the Windows build (and vice-versa) depending on where my development time and tokens go!*
