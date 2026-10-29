# Assert-Signed: throws unless a file carries an intact, timestamped
# Authenticode signature. Dot-sourced by check-signed.ps1 and the install
# tests.
#
# With TEST_CERTIFICATE set to true, a signature whose certificate chains to
# an untrusted root is accepted too: SignPath's test certificate is
# self-signed. The release workflow never sets it for a tag.

function Assert-Signed([string] $Path) {
    $name = Split-Path $Path -Leaf
    $s = Get-AuthenticodeSignature $Path
    # A changed file reads HashMismatch and an unsigned one NotSigned; only
    # a chain problem reads UnknownError.
    $untrustedRoot = $s.Status -eq 'UnknownError' -and $s.SignerCertificate
    if ($env:TEST_CERTIFICATE -eq 'true' -and $untrustedRoot) {
        Write-Host "$name carries the test certificate's signature, which Windows doesn't trust"
    } elseif ($s.Status -ne 'Valid') {
        throw "$name is not validly signed: $($s.Status), $($s.StatusMessage)"
    }
    if (-not $s.TimeStamperCertificate) { throw "$name is signed without a timestamp, so the signature ends with the certificate" }
    Write-Host "$name signed by $($s.SignerCertificate.Subject), timestamped by $($s.TimeStamperCertificate.Subject)"
}
