# Fix Email Delivery for Custom Domains (FastMail)

Domains: **aristotle.ai**, **mattgpt.ai**, **tangibleintelligence.com**

---

## DNS Audit Results (2026-02-16)

### aristotle.ai — BROKEN (pointing to Outlook)

| Record | Status | Current Value | Required Value |
|--------|--------|---------------|----------------|
| MX | **WRONG** | `0 aristotle-ai.mail.protection.outlook.com` | `10 in1-smtp.messagingengine.com` / `20 in2-smtp.messagingengine.com` |
| SPF | **WRONG** | `v=spf1 include:spf.protection.outlook.com include:itga.co -all` | `v=spf1 include:spf.messagingengine.com ~all` |
| DKIM fm1 | **MISSING** | (NXDOMAIN) | CNAME → `fm1.aristotle.ai.dkim.fmhosted.com` |
| DKIM fm2 | **MISSING** | (NXDOMAIN) | CNAME → `fm2.aristotle.ai.dkim.fmhosted.com` |
| DKIM fm3 | **MISSING** | (NXDOMAIN) | CNAME → `fm3.aristotle.ai.dkim.fmhosted.com` |
| DMARC | OK | `v=DMARC1; p=none;` | (keep, or add `rua=mailto:...` for reports) |

### mattgpt.ai — MOSTLY OK (missing DMARC)

| Record | Status | Current Value | Required Value |
|--------|--------|---------------|----------------|
| MX | **OK** | `10 in1-smtp.messagingengine.com` / `20 in2-smtp.messagingengine.com` | (no change) |
| SPF | **OK** | `v=spf1 include:spf.messagingengine.com ?all` | Consider tightening `?all` → `~all` |
| DKIM fm1 | **OK** | CNAME → `fm1.mattgpt.ai.dkim.fmhosted.com` | (no change) |
| DKIM fm2 | **OK** | CNAME → `fm2.mattgpt.ai.dkim.fmhosted.com` | (no change) |
| DKIM fm3 | **OK** | CNAME → `fm3.mattgpt.ai.dkim.fmhosted.com` | (no change) |
| DMARC | **MISSING** | (NXDOMAIN) | `v=DMARC1; p=none; rua=mailto:matt@mattgpt.ai` |

### tangibleintelligence.com — BROKEN (pointing to Outlook/Sendinblue)

| Record | Status | Current Value | Required Value |
|--------|--------|---------------|----------------|
| MX | **WRONG** | `0 tangibleintelligence-com.mail.protection.outlook.com` | `10 in1-smtp.messagingengine.com` / `20 in2-smtp.messagingengine.com` |
| SPF | **WRONG** | `v=spf1 include:spf.protection.outlook.com include:spf.sendinblue.com mx -all` | `v=spf1 include:spf.messagingengine.com ~all` |
| DKIM fm1 | **MISSING** | (NXDOMAIN) | CNAME → `fm1.tangibleintelligence.com.dkim.fmhosted.com` |
| DKIM fm2 | **MISSING** | (NXDOMAIN) | CNAME → `fm2.tangibleintelligence.com.dkim.fmhosted.com` |
| DKIM fm3 | **MISSING** | (NXDOMAIN) | CNAME → `fm3.tangibleintelligence.com.dkim.fmhosted.com` |
| DMARC | STALE | `v=DMARC1; p=none; ... rua=mailto:dmarc@mailinblue.com ...` | `v=DMARC1; p=none; rua=mailto:matt@tangibleintelligence.com` |

---

## Remediation: aristotle.ai

**Registrar:** (check — likely Namecheap, Cloudflare, or Google Domains)

1. **Delete** the existing Outlook MX record
2. **Add** MX records:
   - `10 in1-smtp.messagingengine.com`
   - `20 in2-smtp.messagingengine.com`
3. **Replace** the SPF TXT record on `@` with:
   ```
   v=spf1 include:spf.messagingengine.com ~all
   ```
4. **Add** 3 DKIM CNAME records (confirm exact values in FastMail > Settings > Domains > aristotle.ai):
   - `fm1._domainkey` → `fm1.aristotle.ai.dkim.fmhosted.com`
   - `fm2._domainkey` → `fm2.aristotle.ai.dkim.fmhosted.com`
   - `fm3._domainkey` → `fm3.aristotle.ai.dkim.fmhosted.com`
5. **Update** DMARC TXT on `_dmarc` (optional but recommended):
   ```
   v=DMARC1; p=none; rua=mailto:matt@aristotle.ai
   ```

## Remediation: mattgpt.ai

1. **Add** DMARC TXT record on `_dmarc`:
   ```
   v=DMARC1; p=none; rua=mailto:matt@mattgpt.ai
   ```
2. (Optional) **Tighten** SPF from `?all` to `~all` for better spam protection

## Remediation: tangibleintelligence.com

**Registrar:** (check — likely wherever it was registered for Tangible Intelligence)

1. **Delete** the existing Outlook MX record
2. **Add** MX records:
   - `10 in1-smtp.messagingengine.com`
   - `20 in2-smtp.messagingengine.com`
3. **Replace** the SPF TXT record on `@` with:
   ```
   v=spf1 include:spf.messagingengine.com ~all
   ```
4. **Add** 3 DKIM CNAME records (confirm exact values in FastMail > Settings > Domains):
   - `fm1._domainkey` → `fm1.tangibleintelligence.com.dkim.fmhosted.com`
   - `fm2._domainkey` → `fm2.tangibleintelligence.com.dkim.fmhosted.com`
   - `fm3._domainkey` → `fm3.tangibleintelligence.com.dkim.fmhosted.com`
5. **Replace** DMARC TXT record on `_dmarc` with:
   ```
   v=DMARC1; p=none; rua=mailto:matt@tangibleintelligence.com
   ```

---

## Post-Fix Checklist

After making DNS changes at the registrar(s):

- [ ] Wait for DNS propagation (check at https://dnschecker.org/)
- [ ] Verify green checkmarks in FastMail > Settings > Domains for all 3 domains
- [ ] Confirm aliases exist in FastMail > Settings > Aliases for each address
- [ ] Send test email TO each domain from Gmail — confirm receipt
- [ ] Send test email FROM each domain to Gmail — confirm delivery (check spam)
- [ ] Send calendar invite TO each domain — confirm receipt
- [ ] Send calendar invite FROM each domain — confirm delivery
- [ ] Check deliverability score at https://mail-tester.com/ for each domain
- [ ] LinkedIn > Settings > Sign in & security > Email addresses — remove and re-add bouncing email
- [ ] Confirm LinkedIn bounce warning is gone

---

## Quick Reference: FastMail Required DNS Records

| Record | Type | Name | Value |
|--------|------|------|-------|
| MX | MX | @ | 10 in1-smtp.messagingengine.com |
| MX | MX | @ | 20 in2-smtp.messagingengine.com |
| SPF | TXT | @ | v=spf1 include:spf.messagingengine.com ~all |
| DKIM | CNAME | fm1._domainkey | (from FastMail settings) |
| DKIM | CNAME | fm2._domainkey | (from FastMail settings) |
| DKIM | CNAME | fm3._domainkey | (from FastMail settings) |
| DMARC | TXT | _dmarc | v=DMARC1; p=none; rua=mailto:you@domain |

Docs: https://www.fastmail.com/help/receive/domains.html
