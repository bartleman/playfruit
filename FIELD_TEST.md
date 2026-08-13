# Playfruit field test — 10 minutes, one-paste diagnosable

Run these steps in order on the Windows PC. If any step fails, copy the
output of that step (plus the log files in step 7) into an issue/report —
that's enough to diagnose it.

**Prep**: unzip to a fixed folder you won't move (firewall rules are bound
to the exe path). No IPs needed — devices are found by name.

1. **Start audio playing** on the PC (a looping YouTube video is perfect) and
   leave it playing for the whole test.

2. **Run the tray app**: `playfruit-tray.exe`.
   SmartScreen: *More info → Run anyway*.
   EXPECT: gray circle in the tray (check the `^` overflow near the clock).

3. **Firewall**: on first run the administrator prompt appears by itself —
   approve it. EXPECT: menu shows "Firewall access enabled ✓" within ~10s
   and no other firewall dialogs ever appear.
   (Upgrading from an older version: if the prompt doesn't auto-appear,
   right-click → **Enable firewall access…** once — it upgrades old rules
   to cover Public networks.)

4. **Doctor** (PowerShell in the unzip folder):

   ```
   .\playfruit.exe doctor
   ```

   It finds your HomePods automatically. If it lists more than one, pick by
   name, e.g. `.\playfruit.exe doctor kitchen`.

   EXPECT every line `[PASS]` (capture may `[WARN]` if your audio paused).
   The `clock-sync` check is the important one: `FAIL ... 0 timing requests`
   means the HomePod cannot reach this PC — a firewall/network-profile issue,
   and exactly why a stream would be silent.

5. **Stream**: right-click → *Mirror PC audio to* → your HomePod.
   EXPECT: icon turns **green** and audio comes out of the HomePod within
   ~2s, about half a second behind the PC.
   - **Pale green** icon + "nothing is playing on this PC" = start some audio.
   - **Red** icon = read the status line; it names the problem.

6. **Resilience** (optional but valuable): toggle the PC's Wi-Fi off for 5s,
   back on. EXPECT: yellow icon "reconnecting…", then green again within ~30s.

7. **If anything failed**, collect:
   - the doctor output (step 4),
   - `%APPDATA%\playfruit\playfruit-tray.log` **and**
     `playfruit-tray.prev.log` (copy them before relaunching),
   - `netsh advfirewall firewall show rule name=Playfruit verbose`
   - `powershell -Command "Get-NetConnectionProfile | Select Name,NetworkCategory"`
