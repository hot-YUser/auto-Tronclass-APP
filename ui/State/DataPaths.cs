namespace Ui;

/// <summary>
/// 決定核心資料（config.json / vault.bin / OS-protected device.key.os / providers.json）落在哪
/// —— 全 App 唯一決策點。
///
/// Windows 有兩種形態，由 <c>Ui.exe</c> 旁的 <c>.portable</c> 標記檔決定：
///   • 有標記（免安裝 zip）→ 寫在 exe 旁的 <c>Data\</c>；vault key 仍由目前 Windows
///     使用者的 DPAPI 綁定，因此資料目錄不可跨使用者／裝置解密。
///   • 無標記（Inno 安裝版／開發跑）→ <c>%LOCALAPPDATA%\AutoTronclass\Data</c>（自算路徑，避開 MAUI
///     <c>FileSystem.AppDataDirectory</c> 在未設 Publisher 時產生的裸 "User Name" 資料夾）。
/// 其餘平台（Android/iOS）一律走平台沙盒 <c>FileSystem.AppDataDirectory</c>，不更動。
///
/// 另附一次性搬遷：舊版寫在 MAUI 預設的 <c>%LOCALAPPDATA%\User Name\com.autotronclass.app\Data</c>，
/// 若目的地尚無 vault 而舊位置有，整包複製過來 → 升級後帳號不會不見。搬完保守不刪舊檔。
/// </summary>
public static class DataPaths
{
    public static string Resolve()
    {
        if (!OperatingSystem.IsWindows())
            return FileSystem.AppDataDirectory;

        var exeDir = AppContext.BaseDirectory;

        // 真 portable：exe 旁有 .portable 標記，且該處可寫（解壓到一般使用者資料夾都可寫）。
        if (File.Exists(Path.Combine(exeDir, ".portable")))
        {
            var portable = Path.Combine(exeDir, "Data");
            if (TryEnsureWritable(portable)) { MigrateLegacy(portable); return portable; }
            // 極少數：解壓到唯讀位置（如 Program Files）→ 退回 LocalAppData，App 至少能用（清楚勝過開不了）。
        }

        var local = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "AutoTronclass", "Data");
        Directory.CreateDirectory(local);
        MigrateLegacy(local);
        return local;
    }

    // 建資料夾並實寫一個探針檔確認可寫；任何例外都當作不可寫。
    static bool TryEnsureWritable(string dir)
    {
        try
        {
            Directory.CreateDirectory(dir);
            var probe = Path.Combine(dir, ".wtest");
            File.WriteAllText(probe, "");
            File.Delete(probe);
            return true;
        }
        catch { return false; }
    }

    static void MigrateLegacy(string dest)
    {
        var marker = Path.Combine(dest, ".legacy-migration-complete");
        if (File.Exists(marker) || File.Exists(Path.Combine(dest, "vault.bin"))) return;
        var legacy = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "User Name", "com.autotronclass.app", "Data");
        if (!File.Exists(Path.Combine(legacy, "vault.bin"))) return;

        var stage = Path.Combine(Path.GetDirectoryName(dest)!, $".{Path.GetFileName(dest)}.migration-{Guid.NewGuid():N}");
        var moved = new List<string>();
        try
        {
            Directory.CreateDirectory(stage);
            foreach (var file in Directory.GetFiles(legacy))
                File.Copy(file, Path.Combine(stage, Path.GetFileName(file)), overwrite: false);
            if (!File.Exists(Path.Combine(stage, "vault.bin")))
                throw new InvalidDataException("舊資料缺少 vault.bin");
            var completion = Path.Combine(stage, ".legacy-migration-complete");
            File.WriteAllText(completion, "1");
            foreach (var file in Directory.GetFiles(stage).Where(file => !string.Equals(file, completion, StringComparison.OrdinalIgnoreCase)))
            {
                var target = Path.Combine(dest, Path.GetFileName(file));
                File.Move(file, target, overwrite: false);
                moved.Add(target);
            }
            File.Move(completion, marker, overwrite: false);
            moved.Add(marker);
        }
        catch
        {
            foreach (var partial in moved)
                if (File.Exists(partial) && !File.Exists(marker)) File.Delete(partial);
        }
        finally
        {
            if (Directory.Exists(stage)) Directory.Delete(stage, recursive: true);
        }
    }
}
