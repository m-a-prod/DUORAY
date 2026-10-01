# Server descriptions with Remnawave

Remnawave adds each host's *server description* (`meta.serverDescription` in
JSON, `?serverDescription=` in links) only for "extended clients". It
recognises Happ, INCY, FlClash X and a few others by User-Agent; DUORAY has to
be added by the panel admin.

In the panel: **Subscription settings → Response rules**, add this rule
**before** the final fallback rule:

```json
{
  "name": "DUORAY",
  "description": "DUORAY desktop client: xray JSON with server descriptions",
  "enabled": true,
  "operator": "AND",
  "conditions": [
    { "headerName": "user-agent", "operator": "REGEX", "value": "^Duoray/", "caseSensitive": false }
  ],
  "responseType": "XRAY_JSON",
  "responseModifications": {
    "additionalExtendedClientsRegex": ["^Duoray/"]
  }
}
```

DUORAY requests the subscription URL as given first (response rules apply
there) and only falls back to `<url>/json`, which ignores the rules.
