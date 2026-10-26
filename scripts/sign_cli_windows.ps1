param([Parameter(Mandatory = $true)][string]$Binary)
$ErrorActionPreference = 'Stop'

foreach ($name in @('SIGNING_ENDPOINT', 'SIGNING_ACCOUNT_NAME', 'SIGNING_CERTIFICATE_PROFILE')) {
    if ([string]::IsNullOrWhiteSpace([Environment]::GetEnvironmentVariable($name))) {
        throw "Missing $name"
    }
}
if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) {
    throw 'Missing Synck executable'
}

if (-not (Get-PackageProvider -Name NuGet -ErrorAction SilentlyContinue)) {
    Install-PackageProvider -Name NuGet -Force -Scope CurrentUser | Out-Null
}
if (-not (Get-PSResourceRepository -Name PSGallery -ErrorAction SilentlyContinue)) {
    Register-PSResourceRepository -PSGallery
}
Install-PSResource -Name TrustedSigning -Version 0.5.8 -Repository PSGallery `
    -Scope CurrentUser -TrustRepository -AcceptLicense
Import-Module TrustedSigning -RequiredVersion 0.5.8

$catalog = Join-Path $env:RUNNER_TEMP 'synck-signing-catalog.txt'
(Resolve-Path -LiteralPath $Binary).Path | Set-Content -LiteralPath $catalog -Encoding ascii
$signing = @{
    Endpoint = $env:SIGNING_ENDPOINT
    CodeSigningAccountName = $env:SIGNING_ACCOUNT_NAME
    CertificateProfileName = $env:SIGNING_CERTIFICATE_PROFILE
    FilesCatalog = $catalog
    FileDigest = 'SHA256'
    TimestampRfc3161 = 'http://timestamp.acs.microsoft.com'
    TimestampDigest = 'SHA256'
    Description = 'Synck CLI'
    DescriptionUrl = 'https://github.com/synergyos/synck'
    ExcludeWorkloadIdentityCredential = $true
    ExcludeManagedIdentityCredential = $true
    ExcludeSharedTokenCacheCredential = $true
    ExcludeVisualStudioCredential = $true
    ExcludeVisualStudioCodeCredential = $true
    ExcludeAzurePowerShellCredential = $true
    ExcludeAzureDeveloperCliCredential = $true
    ExcludeInteractiveBrowserCredential = $true
}
Invoke-TrustedSigning @signing

$signature = Get-AuthenticodeSignature -LiteralPath $Binary
if ($signature.Status -ne 'Valid' -or $null -eq $signature.TimeStamperCertificate) {
    throw 'Synck must have a valid Authenticode signature and timestamp'
}
$signtool = Get-ChildItem "${env:ProgramFiles(x86)}\Windows Kits\10\bin" `
    -Recurse -Filter signtool.exe -File |
    Where-Object { $_.FullName -match '\\x64\\signtool\.exe$' } |
    Sort-Object FullName -Descending |
    Select-Object -First 1 -ExpandProperty FullName
if (-not $signtool) {
    throw 'Windows SDK signtool.exe not found'
}
& $signtool verify /pa /v $Binary
if ($LASTEXITCODE -ne 0) {
    throw 'SignTool verification failed'
}
