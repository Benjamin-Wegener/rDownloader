# RDownloader Windows Packaging Notes

For Windows releases:
1. Application icon is located at `packaging/icons/rdownloader.ico`.
2. Windows manifest `packaging/windows/rdownloader.manifest` specifies Per-Monitor DPI V2 awareness and un-elevated execution.
3. Windows registry autostart is implemented in `src/gui/autostart.rs` at:
   `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`
   under key `"RDownloader"`.
4. Windows installer scripts (Inno Setup / WiX) can bundle `rdownloader.exe`, `rdownloader.ico`, and register the `.torrent` / link associations.
