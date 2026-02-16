# Fix Email Delivery for Custom Domains (FastMail)

Domains: **aristotle.ai**, **mattgpt.ai**, **tangibleintelligence.com**

---

## Step 1: Audit DNS Records

For each domain, run these checks (use [MXToolbox](https://mxtoolbox.com/) or `dig`):

```sh
dig +short MX aristotle.ai
dig +short TXT aristotle.ai           # SPF
dig +short TXT fm1._domainkey.aristotle.ai  # DKIM
dig +short TXT _dmarc.aristotle.ai    # DMARC
```

Repeat for mattgpt.ai and tangibleintelligence.com.

---

## Step 2: Fix MX Records

At each domain's registrar, set the MX records to FastMail's servers:

| Priority | Server |
|----------|--------|
| 10 | in1-smtp.messagingengine.com |
| 20 | in2-smtp.messagingengine.com |

Delete any other MX records (e.g. old Google Workspace or default registrar entries).

---

## Step 3: Fix SPF Record

Add (or replace) a single TXT record on each domain's root (`@`):

```
v=spf1 include:spf.messagingengine.com ~all
```

There must be only **one** SPF record per domain. If there's an existing one, merge it or replace it.

---

## Step 4: Fix DKIM Records

In FastMail, go to **Settings > Domains > [domain] > DKIM**. FastMail will show you 3 CNAME records to add. They look like:

| Name | Type | Value |
|------|------|-------|
| fm1._domainkey | CNAME | fm1.aristotle.ai.dkim.fmhosted.com |
| fm2._domainkey | CNAME | fm2.aristotle.ai.dkim.fmhosted.com |
| fm3._domainkey | CNAME | fm3.aristotle.ai.dkim.fmhosted.com |

Copy the exact values from FastMail's domain settings page for each domain — the values differ per domain.

---

## Step 5: Fix DMARC Record

Add a TXT record on `_dmarc` subdomain for each domain:

```
v=DMARC1; p=none; rua=mailto:matt@aristotle.ai
```

Start with `p=none` (monitor mode). Tighten to `p=quarantine` or `p=reject` once delivery is confirmed working.

---

## Step 6: Verify in FastMail

1. Go to **Settings > Domains** in FastMail
2. Click each domain
3. FastMail shows green checkmarks next to each DNS record type when correct
4. Fix any that show red/yellow warnings
5. Make sure each domain shows **Verified**

---

## Step 7: Check Aliases and Routing

In FastMail **Settings > Aliases**:

- Confirm aliases exist for each address you use (e.g. matt@aristotle.ai, matt@mattgpt.ai)
- Check **Settings > Rules** for any forwarding rules — make sure they point to valid destinations

---

## Step 8: Wait for DNS Propagation

DNS changes can take up to 48 hours. Check progress at:
- https://mxtoolbox.com/SuperTool.aspx
- https://dnschecker.org/

---

## Step 9: Test Email Delivery

Send test emails **to and from** each domain:

1. Send from Gmail/other to matt@aristotle.ai — confirm receipt
2. Send from matt@aristotle.ai to Gmail — confirm it arrives (check spam)
3. Repeat for mattgpt.ai and tangibleintelligence.com
4. Send a calendar invite to each address — confirm it arrives
5. Send a calendar invite from each address — confirm delivery

Use https://mail-tester.com/ to check deliverability score from each domain.

---

## Step 10: Fix LinkedIn Bounce Warning

1. Go to LinkedIn > **Settings > Sign in & security > Email addresses**
2. Remove the bouncing email address
3. Re-add it
4. Confirm the verification email arrives
5. Verify the bounce warning is gone

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
