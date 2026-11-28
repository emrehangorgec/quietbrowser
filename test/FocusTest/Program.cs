// Focus test: a text box stays in front and receives typed keys while the browser is
// launched and driven in the background. Any key that does not arrive, or any moment the
// browser becomes the foreground window, counts as a failure.
// Safety: keys are only sent while this window is the foreground window. If any other
// program (not the browser) takes the foreground, the test stops.
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text.Json;

static class Program
{
    [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hWnd);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
    [DllImport("user32.dll")] static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    const uint KEYUP = 0x2;

    static int exit = 1;

    [STAThread]
    static int Main(string[] args)
    {
        ApplicationConfiguration.Initialize();
        var form = new Form { Text = "focus test (do not touch)", Width = 520, Height = 260, StartPosition = FormStartPosition.Manual, Location = new Point(420, 380) };
        var box = new TextBox { Multiline = true, Dock = DockStyle.Fill, ScrollBars = ScrollBars.Vertical };
        form.Controls.Add(box);
        form.Shown += async (_, _) =>
        {
            form.Activate();
            box.Focus();
            try { exit = await RunAsync(form, box, args); }
            catch (Exception ex) { Console.Error.WriteLine(ex); exit = 2; }
            form.Close();
        };
        Application.Run(form);
        return exit;
    }

    static string ProcName(IntPtr hwnd, out uint pid)
    {
        GetWindowThreadProcessId(hwnd, out pid);
        try { return Process.GetProcessById((int)pid).ProcessName; } catch { return "?"; }
    }

    static async Task<int> RunAsync(Form form, TextBox box, string[] args)
    {
        string exe = args[0];
        bool normal = args.Contains("--normal");
        int rounds = args.Contains("--rounds") ? int.Parse(args[Array.IndexOf(args, "--rounds") + 1]) : 100;
        string outFile = args.Contains("--out") ? args[Array.IndexOf(args, "--out") + 1] : "";
        IntPtr me = form.Handle;

        var steals = new List<string>();
        string? abort = null;
        int sent = 0;
        bool stop = false;
        Process? browser = null;

        var poll = new Thread(() =>
        {
            while (!stop)
            {
                var f = GetForegroundWindow();
                if (f != IntPtr.Zero && f != me)
                {
                    var name = ProcName(f, out var pid);
                    bool fromBrowser = name.StartsWith("quietbrowser") || name.StartsWith("msedge") || (browser != null && pid == browser.Id);
                    if (!fromBrowser) { abort = $"foreground taken by {name}"; stop = true; break; }
                    lock (steals) steals.Add($"{DateTime.Now:HH:mm:ss.fff} {name}");
                    SetForegroundWindow(me); // take it back so later steals are counted too
                    Thread.Sleep(30);
                }
                Thread.Sleep(3);
            }
        }) { IsBackground = true };

        var typer = new Thread(() =>
        {
            while (!stop)
            {
                if (GetForegroundWindow() == me)
                {
                    keybd_event(0x41, 0, 0, UIntPtr.Zero);
                    keybd_event(0x41, 0, KEYUP, UIntPtr.Zero);
                    Interlocked.Increment(ref sent);
                }
                Thread.Sleep(12);
            }
        }) { IsBackground = true };

        poll.Start();
        bool noType = args.Contains("--notype"); // Windows blocks focus changes while the user is typing; this checks the idle case
        if (!noType) typer.Start();

        // "edge" = baseline: real Microsoft Edge driven over its DevTools HTTP endpoint,
        // opening each page in a new tab the way automation tools open a new page.
        bool edge = exe == "edge";
        var http = new HttpClient { Timeout = TimeSpan.FromSeconds(30) };
        http.DefaultRequestHeaders.Add("X-QuietBrowser-Client", "1");
        if (edge)
        {
            var profile = Path.Combine(Path.GetTempPath(), "qb-focustest-edge");
            var psiE = new ProcessStartInfo(@"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe") { UseShellExecute = false };
            foreach (var a in new[] { $"--user-data-dir={profile}", "--remote-debugging-port=9333", "--no-first-run", "--no-default-browser-check", "about:blank" })
                psiE.ArgumentList.Add(a);
            browser = Process.Start(psiE)!;
            http.BaseAddress = new Uri("http://localhost:9333/");
            for (int i = 0; ; i++)
            {
                try { await http.GetStringAsync("json/version"); break; }
                catch { if (i > 200) throw new Exception("edge did not start"); await Task.Delay(100); }
            }
        }
        else
        {
            // CreateNoWindow: quietbrowser is a console program, and a new console window would take focus itself.
            var psi = new ProcessStartInfo(exe) { UseShellExecute = false, CreateNoWindow = true };
            psi.ArgumentList.Add("serve");
            psi.ArgumentList.Add("--port"); psi.ArgumentList.Add("9555");
            if (normal) psi.ArgumentList.Add("--normal");
            browser = Process.Start(psi)!;
            http.BaseAddress = new Uri("http://localhost:9555/");
            for (int i = 0; ; i++)
            {
                try { await http.GetStringAsync("hwnd"); break; }
                catch { if (i > 200) throw new Exception("browser did not start"); await Task.Delay(100); }
            }
        }

        string Data(string html) => "data:text/html," + Uri.EscapeDataString(html);
        var pages = new[]
        {
            Data("<title>autofocus</title><input id=q autofocus><button id=b onclick=\"document.title='clicked'\">go</button><script>window.focus();q.focus()</script>"),
            Data("<title>dialogs</title><script>alert('hi');confirm('sure?');prompt('name?')</script>"),
            Data("<title>popup</title><script>window.open('about:blank','_blank')</script>"),
            Data("<title>focus loop</title><input id=q><script>setInterval(()=>{window.focus();q.focus()},50)</script>"),
            "https://example.com/",
        };

        int commands = 0, failed = 0;
        async Task Cmd(string path)
        {
            if (stop) return;
            commands++;
            try { await http.GetStringAsync(path); } catch { failed++; }
        }

        string? lastTab = null;
        for (int r = 0; r < rounds && !stop && edge; r++)
        {
            commands++;
            try
            {
                var res = await http.SendAsync(new HttpRequestMessage(HttpMethod.Put, "json/new?" + pages[r % pages.Length]));
                var id = JsonDocument.Parse(await res.Content.ReadAsStringAsync()).RootElement.GetProperty("id").GetString();
                await Task.Delay(400);
                if (lastTab != null) await http.GetStringAsync("json/close/" + lastTab);
                lastTab = id;
            }
            catch { failed++; }
        }

        for (int r = 0; r < rounds && !stop && !edge; r++)
        {
            var page = pages[r % pages.Length];
            await Cmd("open?url=" + Uri.EscapeDataString(page));
            if (r % pages.Length == 0)
            {
                await Cmd("fill?sel=%23q&text=hello");
                await Cmd("click?sel=%23b");
            }
            await Cmd("eval?js=" + Uri.EscapeDataString("window.focus();document.body&&document.body.focus();1"));
        }

        await Task.Delay(500);
        stop = true;
        if (!noType) typer.Join();
        poll.Join();
        await Task.Delay(300);
        Application.DoEvents();
        int received = box.TextLength;
        string dialogs = "";
        if (edge) { try { browser.Kill(true); } catch { } }
        else try { dialogs = await http.GetStringAsync("dialogs"); await http.GetStringAsync("quit"); } catch { }

        var result = new
        {
            mode = edge ? "edge (baseline)" : normal ? "normal (control)" : "quiet",
            rounds,
            commands,
            failedCommands = failed,
            keysSent = sent,
            keysReceived = received,
            keysLost = sent - received,
            focusSteals = steals.Count,
            firstSteals = steals.Take(5),
            dialogsAnsweredInCode = dialogs,
            aborted = abort,
        };
        var json = JsonSerializer.Serialize(result, new JsonSerializerOptions { WriteIndented = true });
        if (outFile != "") File.WriteAllText(outFile, json);
        Console.WriteLine(json);
        return abort == null && steals.Count == 0 && sent == received ? 0 : 1;
    }
}
