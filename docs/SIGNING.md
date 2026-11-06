# Signing the Windows and macOS builds

Status for 0.2.0: **neither download is signed by a publisher yet.** The macOS app is ad-hoc
signed (Apple Silicon needs some signature to run at all) and not notarized; the Windows exe isn't
code-signed. Both work, but the first launch needs one extra click, which each download's
`READ ME FIRST.txt` explains.

The release workflow is already wired for macOS signing and notarization and for Windows signing
through Azure Artifact Signing: each switches on by itself once its repository secrets exist.
Nothing else in the repo has to change.

**Shared with Plaza.** Tailgate uses the same Apple team, the same Developer ID Application
certificate, the same notary API key and the same Azure signing account as
[Plaza](https://github.com/nunyabiz765/plaza). The secret names are identical in both repos, so
the setup is done once and the same values go into both. Don't create a second certificate.

## macOS: Developer ID + notarization

Apple team: **X993AH3NS8** (the same team as the Alderlight app). The Mac's keychain currently
holds only an *Apple Development* certificate, which can't sign apps for download. Two things are
missing, and both need the Apple Developer **Account Holder** (Nick) signed in:

1. **A Developer ID Application certificate.** Easiest on the Mac, in Xcode:
   Xcode > Settings > Accounts > select the team > Manage Certificates… > **+** >
   **Developer ID Application**. Xcode makes the key and certificate and puts both in the login
   keychain. (Only the Account Holder sees that option.)
   Then export it for CI: Keychain Access > login > My Certificates > right-click
   "Developer ID Application: …" > Export… > save as `.p12` with a password.
2. **An App Store Connect API key for notarization.** appstoreconnect.apple.com > Users and
   Access > Integrations > App Store Connect API > Team Keys > **+** (role: Developer). Download
   the `.p8` (it can only be downloaded once) and note the **Key ID** and **Issuer ID**.

Then the repository secrets (Settings > Secrets and variables > Actions, or `gh secret set NAME <
file`, so no value is ever printed):

| Secret | Value |
|---|---|
| `MACOS_CERT_P12` | `base64 -i DeveloperID.p12` |
| `MACOS_CERT_PASSWORD` | the export password |
| `NOTARY_KEY_P8` | `base64 -i AuthKey_XXXXXX.p8` |
| `NOTARY_KEY_ID` | the Key ID |
| `NOTARY_ISSUER_ID` | the Issuer ID |

`MACOS_SIGN_IDENTITY` is optional: CI reads the "Developer ID Application: …" name from the .p12.

The next tag then signs with the hardened runtime and a timestamp, notarizes the `.dmg`, staples
the ticket and checks it with `spctl`. Released assets are never replaced, so a signed build ships
as the next version (for example 0.2.1), not over 0.2.0's files.

Locally on the Mac, the same script works without CI:
`MACOS_SIGN_IDENTITY="Developer ID Application: …" NOTARY_PROFILE=tailgate-notary tools/package-macos.sh …`
after a one-time `xcrun notarytool store-credentials tailgate-notary --key … --key-id … --issuer …`.
Over SSH the login keychain is locked, so codesign fails with `errSecInternalComponent` unless the
keychain is unlocked in the GUI session first.

Cost: nothing extra; the Apple Developer Program membership already covers Developer ID.

## Windows: Azure Artifact Signing (approved; Nick sets up the account)

Nick chose Azure Artifact Signing (formerly Trusted Signing), Basic tier. The click-by-click setup
is the same as Plaza's (`~/Downloads/plaza/SIGNING-WINDOWS.md` on obsidian2); one signing account
and one certificate profile cover both apps. The workflow then signs `tailgate.exe` with
`azure/artifact-signing-action@v2` (SHA256, Microsoft timestamp) before zipping and fails the
build unless `Get-AuthenticodeSignature` reports **Valid**. Repository secrets:

| Secret | Value |
|---|---|
| `AZURE_TENANT_ID` | Directory (tenant) ID |
| `AZURE_CLIENT_ID` | Application (client) ID of the GitHub signing app |
| `AZURE_CLIENT_SECRET` | that app's client secret value |
| `AZURE_SIGNING_ENDPOINT` | e.g. `https://eus.codesigning.azure.net/` (must match the account's region) |
| `AZURE_SIGNING_ACCOUNT` | the signing account name |
| `AZURE_CERT_PROFILE` | the certificate profile name |

## Windows: the options that were compared (nothing bought)

SmartScreen warns about any new download until it builds a reputation. A signature names the
publisher, and reputation then accrues to that publisher instead of to each file. Since 2024 no
certificate type skips the warning on day one, EV included.

| Option | Cost | Notes |
|---|---|---|
| **Azure Artifact Signing** (was Trusted Signing), Basic | $9.99/month, 5,000 signatures | Microsoft's own CA, cheapest publicly-trusted option. Individuals: US/Canada only, validated against the Azure billing account's legal name and a government ID. The LLC could validate as an organization instead (Microsoft has asked organizations for a business history; a 2026 LLC may not pass yet). Signs from GitHub Actions with `azure/trusted-signing-action`. Certificates are short-lived and renew themselves. |
| **OV code-signing certificate** (Sectigo, DigiCert, SSL.com, Certum…) | roughly $200–500/year | Since 2023 the key has to live on a hardware token or a cloud HSM (e.g. SSL.com eSigner, which also works from CI, with its own fee). Shows NICKSOSA.TECH LLC as publisher. |
| **Certum Open Source Code Signing** | about €30–70/year plus a smart-card reader | For open-source individuals; shows "Open Source Developer, Nicholas Sosa". Card-based, so signing happens on a machine with the card, not in CI. |
| **SignPath Foundation** | free for qualifying open-source projects | Application and review; they sign from CI using their certificate. Projects usually need some existing users and history. |
| **Stay unsigned** | free | Current state. "More info" > "Run anyway" once per copy; documented in `READ ME FIRST.txt`. |

Recommendation: Azure Artifact Signing Basic if paying is fine. It's the cheapest real signature,
it runs entirely in CI, and the yearly cost (~$120) is below any OV certificate. Validate as an
individual if Microsoft won't take the LLC yet. Until then, unsigned with clear instructions is honest and works.
