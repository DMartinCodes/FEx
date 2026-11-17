# FEx
A file explorer that tries to be faster



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
