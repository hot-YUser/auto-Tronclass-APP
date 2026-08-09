using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Security.Cryptography;

namespace TronClass.Interop;

/// <summary>Windows CurrentUser DPAPI envelope; no package and no extractable wrapping key.</summary>
internal static class DeviceKeyProtector
{
    private const uint CryptProtectUiForbidden = 0x1;
    private const uint MoveFileWriteThrough = 0x8;
    private const int ErrorFileExists = 80;
    private const int ErrorAlreadyExists = 183;
    private static ReadOnlySpan<byte> Magic => "ATK1W"u8;

    public static byte[] Protect(ReadOnlySpan<byte> plaintext)
    {
        var protectedBytes = Transform(plaintext, protect: true);
        var envelope = new byte[Magic.Length + protectedBytes.Length];
        Magic.CopyTo(envelope);
        protectedBytes.CopyTo(envelope.AsSpan(Magic.Length));
        CryptographicOperations.ZeroMemory(protectedBytes);
        return envelope;
    }

    public static byte[] Unprotect(ReadOnlySpan<byte> envelope)
    {
        if (!envelope.StartsWith(Magic))
            throw new InvalidDataException("裝置金鑰不是 Windows DPAPI 格式。");
        return Transform(envelope[Magic.Length..], protect: false);
    }

    /// <summary>Atomically create the final envelope and durably flush the NTFS rename metadata.</summary>
    public static bool CommitNew(string temporaryPath, string destination)
    {
        if (MoveFileEx(temporaryPath, destination, MoveFileWriteThrough)) return true;
        var error = Marshal.GetLastWin32Error();
        if (error is ErrorFileExists or ErrorAlreadyExists) return false;
        throw new Win32Exception(error);
    }

    private static unsafe byte[] Transform(ReadOnlySpan<byte> input, bool protect)
    {
        fixed (byte* pointer = input)
        {
            var source = new DataBlob { Size = (uint)input.Length, Data = (nint)pointer };
            DataBlob output;
            nint description = 0;
            var succeeded = protect
                ? CryptProtectData(ref source, "AutoTronclass vault key", 0, 0, 0,
                    CryptProtectUiForbidden, out output)
                : CryptUnprotectData(ref source, out description, 0, 0, 0,
                    CryptProtectUiForbidden, out output);
            if (!succeeded) throw new Win32Exception(Marshal.GetLastWin32Error());

            try
            {
                var result = new byte[checked((int)output.Size)];
                Marshal.Copy(output.Data, result, 0, result.Length);
                return result;
            }
            finally
            {
                if (output.Data != 0)
                {
                    new Span<byte>((void*)output.Data, checked((int)output.Size)).Clear();
                    LocalFree(output.Data);
                }
                if (description != 0) LocalFree(description);
            }
        }
    }

    [StructLayout(LayoutKind.Sequential)]
    private struct DataBlob
    {
        public uint Size;
        public nint Data;
    }

    [DllImport("crypt32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CryptProtectData(
        ref DataBlob dataIn, string? description, nint optionalEntropy, nint reserved,
        nint prompt, uint flags, out DataBlob dataOut);

    [DllImport("crypt32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CryptUnprotectData(
        ref DataBlob dataIn, out nint description, nint optionalEntropy, nint reserved,
        nint prompt, uint flags, out DataBlob dataOut);

    [DllImport("kernel32.dll")]
    private static extern nint LocalFree(nint memory);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool MoveFileEx(string existingPath, string newPath, uint flags);
}
