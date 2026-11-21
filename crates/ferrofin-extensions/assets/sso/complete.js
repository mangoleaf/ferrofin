'use strict';
// The Single Sign-On completion page's script: swaps the page's one-time
// exchange code for a session at `POST ../exchange` (the flow cookie rides
// along), keeps the session where jellyfin-web keeps its own, and opens
// jellyfin-web. Its paths are relative, so a server under a base URL keeps
// working. The token never reaches a URL.
(async () => {
    const status = document.getElementById('status');
    const failed = (title, message) => {
        document.title = document.querySelector('h1').textContent = title;
        status.textContent = message;
    };
    try {
        // The device as jellyfin-web's apphost.js names it: a wrapper app's
        // own, else the id under `_deviceId2`, made as apphost.js makes one
        // when there is none.
        const shell = window.NativeShell && window.NativeShell.AppHost;
        let deviceId = shell && shell.deviceId ? shell.deviceId() : localStorage.getItem('_deviceId2');
        if (!deviceId) {
            deviceId = btoa([navigator.userAgent, new Date().getTime()].join('|')).replaceAll('=', '1');
            localStorage.setItem('_deviceId2', deviceId);
        }
        // jellyfin-web sends its own device name and version with the token,
        // and the server updates the device to them.
        const client = shell && shell.appName ? shell.appName() : 'Jellyfin Web';
        const device = shell && shell.deviceName ? shell.deviceName() : 'Web Browser';
        const version = shell && shell.appVersion ? shell.appVersion() : status.dataset.version;
        const field = (name, value) => name + '="' + encodeURIComponent(value) + '"';
        const response = await fetch('../exchange', {
            method: 'POST',
            credentials: 'same-origin',
            headers: {
                'Content-Type': 'application/json',
                Authorization: 'MediaBrowser ' + [
                    field('Client', client),
                    field('Device', device),
                    field('DeviceId', deviceId),
                    field('Version', version)
                ].join(', ')
            },
            body: JSON.stringify({ Code: status.dataset.code })
        });
        if (response.status === 403) {
            // A refusal by the account's sign-in rules says which.
            failed('Sign-in refused', await response.text());
            return;
        }
        if (response.status === 429) {
            failed('Too many sign-in attempts',
                'Too many sign-in attempts came from your network. Wait a minute, then try again.');
            return;
        }
        if (!response.ok) {
            failed('Sign-in failed', 'Sign-in failed. Try again.');
            return;
        }
        const result = await response.json();
        // jellyfin-web's servers and tokens: jellyfin-apiclient's
        // `Credentials`, JSON under `jellyfin_credentials`. jellyfin-web finds
        // this server in it by its address, the page's URL up to `/sso/`.
        const address = new URL('../../../', location.href).href.replace(/\/$/, '');
        const credentials = JSON.parse(localStorage.getItem('jellyfin_credentials') || '{}');
        credentials.Servers = credentials.Servers || [];
        let server = credentials.Servers.find((known) => known.Id === result.ServerId);
        if (!server) {
            server = { Id: result.ServerId };
            credentials.Servers.push(server);
        }
        Object.assign(server, {
            ManualAddress: address,
            LastConnectionMode: 2,
            DateLastAccessed: Date.now(),
            UserId: result.User.Id,
            AccessToken: result.AccessToken
        });
        localStorage.setItem('jellyfin_credentials', JSON.stringify(credentials));
        location.replace('../../../web/');
    } catch {
        failed('Sign-in failed', 'Sign-in failed. Try again.');
    }
})();
