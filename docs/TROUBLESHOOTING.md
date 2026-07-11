# Troubleshooting

## "KiCAD IPC socket path not configured"

Any tool that talks to a live KiCAD session (`save_project`, PCB editing,
`check_kicad_ui`, …) needs the IPC socket address. Two separate configurations
must both be correct — neither happens automatically:

1. **The socket path in Konnect's plugin settings** (inside KiCAD)
2. **The Konnect server registration in your AI client's MCP config**

Step by step (based on the diagnostic guide contributed in
[#18](https://github.com/mixelpixx/Konnect/issues/18)):

1. Open KiCAD normally.
2. Go to **Edit → Preferences → Plugins** and check **"Enable KiCad API"**.
   Confirm a line like this appears:

   ```
   Listening on ipc://C:\Users\<you>\AppData\Local\Temp\kicad\api.sock
   ```

   Copy the whole address including the `ipc://` prefix — it is unique to
   your machine and user.
3. In KiCAD, open **Tools → External Plugins → Konnect** to open the settings
   dialog.
4. Paste the address into the **IPC Socket** field and click **Save**.
5. Confirm your AI client (Claude Code, Claude Desktop, …) has the `konnect`
   MCP server registered in its own config (`.mcp.json` or
   `claude_desktop_config.json`) pointing at the `konnect` binary — see
   [examples/](../examples/). This registration is separate from the KiCAD
   plugin settings.
6. Restart the AI client session so it spawns a fresh Konnect process that
   reads the saved settings.
7. Verify: have the AI call `open_project`. Expected:

   ```json
   { "kicad_ui_running": true, "message": "KiCAD is running and IPC is available." }
   ```

Alternative: launching the server from within KiCAD sets `KICAD_API_SOCKET`
automatically, and a `konnect-settings.json` passed via `--config` can carry
`ipc_socket_path` directly.

## PCB tools return "IPC connect failed" / "No PCB document is open"

The IPC tools talk to KiCAD's **running PCB editor**. Open your board file in
KiCAD first, and make sure the API is enabled (previous section).

## "kicad-cli not found"

Common install paths are auto-detected (including the Windows registry). If
your install is somewhere unusual, set the path in the plugin settings dialog
or in `konnect-settings.json` (`kicad_cli`).

## Plugin doesn't appear in KiCAD

Install via **Plugin and Content Manager → Install from File** with the
`konnect-pcm-*.zip` release asset (not the bare binary archives), then restart
KiCAD.
