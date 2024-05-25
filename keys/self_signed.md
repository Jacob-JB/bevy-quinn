# Windows
using the openssl install provided by git for windows

`'C:\Program Files\Git\usr\bin\openssl.exe' req -x509 -newkey rsa:4096 -keyout key.pem -out cert.pem -sha256 -days 3650 -nodes -subj "/C=AU/ST=Some-State/CN=localhost"`
