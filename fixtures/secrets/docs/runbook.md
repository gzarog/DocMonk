# Billing runbook

## Database access

The billing worker connects with

    postgres://ops:PlantedUrlPw5512@db01.internal:5432/billing

and the mail relay reads its password from the environment:

```sh
export SMTP_PASSWORD=PlantedEnvPw6620
```

## Deploy key

The deploy key for the release host:

```
-----BEGIN RSA PRIVATE KEY-----
MIIEPlantedKeyBodyb2C3d4A1b2C3d4A1b2C3d4A1b2C3d4A1b2C3d4A1b2C3d4
PlantedKeyBodyLineTwo0PlantedKeyBodyLineTwo0PlantedKeyBodyLineTw
PlantedKeyBodyLineThreePlantedKeyBodyLineThreePlantedKeyBodyLine
PlantedKeyBodyTail==
-----END RSA PRIVATE KEY-----
```

Rotate it every quarter.
