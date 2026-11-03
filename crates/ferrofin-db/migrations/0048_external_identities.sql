-- 0048: add FerrofinExternalIdentities, the links from an outside identity to a user.
--
-- An identity is a provider id, an issuer and a subject: LDAP-Auth's uid map (the
-- provider id, an empty issuer, the LDAP uid), which upstream keeps in its plugin
-- configuration as LdapUsers, and later OIDC's issuer and sub. One identity belongs to
-- at most one user, and a user's links go with the user. The store that writes it
-- (external_identity_repository.rs) keeps upstream's PluginConfiguration.AddUser rule:
-- in one transaction it removes the user's link for that provider and any link of that
-- identity, then inserts.
--
-- Purely additive: the table starts empty and the previous version never reads it.
CREATE TABLE "FerrofinExternalIdentities" (
    "Id" INTEGER PRIMARY KEY,
    "UserId" TEXT NOT NULL REFERENCES "Users" ("Id") ON DELETE CASCADE,
    "ProviderId" TEXT NOT NULL,
    "Issuer" TEXT NOT NULL DEFAULT '',
    "Subject" TEXT NOT NULL,
    "AvatarHash" TEXT NULL,
    "DateCreated" TEXT NOT NULL
);
CREATE UNIQUE INDEX "FerrofinIX_ExternalIdentities_Provider_Issuer_Subject"
    ON "FerrofinExternalIdentities" ("ProviderId", "Issuer", "Subject");
CREATE INDEX "FerrofinIX_ExternalIdentities_UserId"
    ON "FerrofinExternalIdentities" ("UserId");
