# WordPress Multisite layout

This folder proves the generic pieces WordPress needs: ordered static routes, a FastCGI fallback, a front controller, and parent-domain handling for subdomains.

To use a real WordPress Multisite installation:

1. Install PHP CGI and the extensions required by the chosen WordPress version. On Alpine, the executable may be versioned (for example `php83-cgi`); update `serve.command` accordingly.
2. Replace the contents of `public/` with the WordPress distribution.
3. Configure the database and Multisite constants in `public/wp-config.php`.
4. Point the base domain and wildcard subdomains through Cloudflare Tunnel to `multi-server`.
5. Rename this directory to the base domain, such as `rotava.com`, and keep `"subdomains": true`.

Requests for common static extensions are served directly. Existing `.php` paths are executed as their corresponding scripts; every other path is sent to `public/index.php`, which provides WordPress-style permalink routing without interpreting `.htaccess`.
