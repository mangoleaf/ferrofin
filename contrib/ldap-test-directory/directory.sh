#!/usr/bin/env bash
# A throwaway OpenLDAP directory to try Ferrofin's LDAP-Auth against, from
# jellyfin-web or a native client. See README.md beside this script.
#
#   directory.sh up [--host NAME] [--bind ADDR] [--dir DIR]
#   directory.sh settings [--dir DIR]
#   directory.sh logs | down
#   directory.sh modify            # LDIF on stdin, as the directory administrator
#   directory.sh picture UID FILE  # replace a person's jpegPhoto with FILE
#
# Two containers: the directory (plain LDAP and StartTLS on 3389, LDAPS on
# 3636) and a partners directory it refers searches to (4389, 4636). Test data
# only: the passwords below are fixed and public. Never point a server you use
# for real at it.
set -euo pipefail

# The image and Dockerfile of crates/ferrofin-extensions/tests/ldap_directory.rs,
# which builds the same tag: keep the two in step.
IMAGE=ferrofin-ldap-test:alpine3.22-openldap
MAIN=ferrofin-ldap-test-directory
PARTNERS=ferrofin-ldap-test-partners
SUFFIX=dc=example,dc=org
ADMIN_DN=cn=admin,$SUFFIX
ADMIN_PASSWORD=admin-secret
BIND_DN=uid=jellyfin,ou=people,$SUFFIX
BIND_PASSWORD=jellyfin-bind-secret
LDAP_PORT=3389
LDAPS_PORT=3636
PARTNERS_LDAP_PORT=4389
PARTNERS_LDAPS_PORT=4636

HOST=localhost
BIND=127.0.0.1
# Holds the CA's and the server's private keys: kept out of any checkout.
DIR=${FERROFIN_LDAP_TEST_DIR:-${XDG_STATE_HOME:-$HOME/.local/state}/ferrofin-ldap-test-directory}

die() {
  echo "directory.sh: $*" >&2
  exit 1
}

# Runs a command, showing its output only when it fails.
quiet() {
  local out
  if ! out=$("$@" 2>&1); then
    printf '%s\n' "$out" >&2
    die "$1 failed"
  fi
}

# Ada's picture: a 64x64 JPEG.
ADA_JPEG=/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAoHBwgHBgoICAgLCgoLDhgQDg0NDh0VFhEYIx8lJCIfIiEmKzcvJik0KSEiMEExNDk7Pj4+JS5ESUM8SDc9Pjv/2wBDAQoLCw4NDhwQEBw7KCIoOzs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozs7Ozv/wAARCABAAEADASIAAhEBAxEB/8QAGQABAQEBAQEAAAAAAAAAAAAAAAUGBwID/8QALxAAAQMDAgIIBgMAAAAAAAAAAQACAwQFEQYSIUEHExQiMVGBkRUkMkNxgqHR0v/EABgBAQEAAwAAAAAAAAAAAAAAAAADAQIF/8QAGxEAAwADAQEAAAAAAAAAAAAAAAECAxFRITH/2gAMAwEAAhEDEQA/AIqIi7RwQiIgCIiAIiIAiIgCpaetTr1eqeiAOxzt0hHJg4n+vVTVsLAPgWj7hfD3aiq+XpjzHmR65P6qeStT59KY5VV78RQ1rVQXTS8VTTNAipq4wt2+G0Bwz/AXP1rqVvW9FtaT9mtBHuz/AEsitcS0muM2zPbVdQREViIREQHqON80rIoxl73BrR5kro9/i03T0VDY7pX1MHY4g4Np2/USMZPdPHgfdYKz1UFDeKSrqWvdFBK2RwYAScHIxkjmvvqK6MvN8qK+MPbHIQGNeACAABy/CjcuqXEWi1EPrNdPDaIOji5iz1E89OZmlzpxg790fDwHDGFz5XxfqVmiTZI45u0STdZI4gbMZzwOc8hyUBZxS53vpjLSrWuBERVJBERAEREAREQBERAf/9k=

# Both directories hold the same suffix and the bind account. The bind account
# reads everything and may set passwords, which "Allow Password Change" needs.
slapd_conf() {
  cat <<EOF
include /etc/openldap/schema/core.schema
include /etc/openldap/schema/cosine.schema
include /etc/openldap/schema/inetorgperson.schema
modulepath /usr/lib/openldap
moduleload back_mdb.so
pidfile /tmp/slapd.pid
argsfile /tmp/slapd.args
TLSCACertificateFile /conf/ca.pem
TLSCertificateFile /conf/server.pem
TLSCertificateKeyFile /conf/server.key
TLSVerifyClient never
database mdb
maxsize 104857600
suffix "$SUFFIX"
rootdn "$ADMIN_DN"
rootpw $ADMIN_PASSWORD
directory /tmp/ldapdata
access to attrs=userPassword by self write by dn.exact="$BIND_DN" write by anonymous auth by * none
access to * by * read
EOF
}

person() { # uid cn sn employeeType password [extra LDIF lines]
  printf 'dn: uid=%s,%s\nobjectClass: inetOrgPerson\nuid: %s\ncn: %s\nsn: %s\nmail: %s@example.org\n' \
    "$1" "${7:-ou=people,$SUFFIX}" "$1" "$2" "$3" "$1"
  if [[ -n $4 ]]; then printf 'employeeType: %s\n' "$4"; fi
  printf 'userPassword: %s\n' "$5"
  if [[ -n ${6:-} ]]; then printf '%s\n' "$6"; fi
  printf '\n'
}

base_seed() {
  cat <<EOF
dn: $SUFFIX
objectClass: top
objectClass: dcObject
objectClass: organization
o: Example
dc: example

dn: ou=people,$SUFFIX
objectClass: organizationalUnit
ou: people

dn: $BIND_DN
objectClass: account
objectClass: simpleSecurityObject
uid: jellyfin
userPassword: $BIND_PASSWORD

EOF
}

# The directory: Ada (an administrator, with a picture), Alan (a user),
# Edsger (not let in by the user filter), Grace's password, and a referral to
# the partners directory, which lists Grace. OpenLDAP checks a user's password
# where the user's DN is, so Grace's credentials sit here in an entry the user
# filter does not find, as an Active Directory forest would check them.
main_seed() {
  base_seed
  person ada "Ada Lovelace" Lovelace jellyfin-admin ada-secret "jpegPhoto:: $ADA_JPEG"
  person alan "Alan Turing" Turing jellyfin-user alan-secret
  person edsger "Edsger Dijkstra" Dijkstra "" edsger-secret
  cat <<EOF
dn: ou=partners,$SUFFIX
objectClass: organizationalUnit
ou: partners

dn: uid=grace,ou=partners,$SUFFIX
objectClass: account
objectClass: simpleSecurityObject
uid: grace
userPassword: grace-secret

dn: ou=partners-referral,$SUFFIX
objectClass: referral
objectClass: extensibleObject
ou: partners-referral
ref: ldap://$HOST:$PARTNERS_LDAP_PORT/ou=partners,$SUFFIX
ref: ldaps://$HOST:$PARTNERS_LDAPS_PORT/ou=partners,$SUFFIX
EOF
}

partners_seed() {
  base_seed
  cat <<EOF
dn: ou=partners,$SUFFIX
objectClass: organizationalUnit
ou: partners

EOF
  person grace "Grace Hopper" Hopper jellyfin-user grace-secret "" "ou=partners,$SUFFIX"
}

is_address() { [[ $1 =~ ^[0-9.]+$ || $1 == *:* ]]; }

certificates() {
  local dir=$1 name=$2 san
  if is_address "$name"; then san="IP:$name"; else san="DNS:$name"; fi
  [[ $name == localhost ]] || san="$san,DNS:localhost"
  quiet openssl req -x509 -newkey rsa:2048 -nodes -days 30 -subj "/CN=Ferrofin LDAP test CA" \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign,cRLSign \
    -keyout "$dir/ca.key" -out "$dir/ca.pem"
  quiet openssl req -newkey rsa:2048 -nodes -subj "/CN=$name" \
    -keyout "$dir/server.key" -out "$dir/server.csr"
  printf 'subjectAltName=%s,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nbasicConstraints=CA:FALSE\n' "$san" \
    >"$dir/server.ext"
  quiet openssl x509 -req -days 30 -in "$dir/server.csr" -CA "$dir/ca.pem" -CAkey "$dir/ca.key" \
    -CAcreateserial -extfile "$dir/server.ext" -out "$dir/server.pem"
  rm -f "$dir/server.csr" "$dir/server.ext" "$dir/ca.srl"
}

start() { # name conf-dir ldap-port ldaps-port
  docker rm -f "$1" >/dev/null 2>&1 || true
  docker run -d --name "$1" -p "$BIND:$3:389" -p "$BIND:$4:636" -v "$2:/conf:ro" "$IMAGE" sh -c \
    'set -e; mkdir -p /tmp/ldapdata; slapadd -f /conf/slapd.conf -l /conf/seed.ldif
     exec slapd -d stats -f /conf/slapd.conf -h "ldap:/// ldaps:///"' >/dev/null
  for _ in $(seq 50); do
    docker exec "$1" ldapsearch -x -H ldap://localhost -b '' -s base >/dev/null 2>&1 && return
    sleep 0.2
  done
  docker logs "$1" >&2
  die "$1 did not start"
}

settings() {
  [[ -f $DIR/host ]] || die "no directory under $DIR: run 'directory.sh up' first"
  DIR=$(cd "$DIR" && pwd)
  HOST=$(cat "$DIR/host")
  cat <<EOF
Dashboard -> Plugins -> LDAP-Auth: enable it, then Settings.

  LDAP Server:                 $HOST
  LDAP Port:                   $LDAP_PORT  (Secure LDAP: $LDAPS_PORT; StartTLS uses $LDAP_PORT)
  LDAP Root CA Path:           $DIR/main/ca.pem  (for Secure LDAP or StartTLS)
  LDAP Bind User:              $BIND_DN
  LDAP Bind User Password:     $BIND_PASSWORD
  LDAP Base DN for searches:   ou=people,$SUFFIX  ($SUFFIX for the referral)
  LDAP Search Filter:          (employeeType=jellyfin*)
  LDAP Search Attributes:      uid, cn, mail
  LDAP Uid Attribute:          uid
  LDAP Username Attribute:     cn
  LDAP Admin Filter:           (employeeType=jellyfin-admin)
  LDAP Profile Image Attribute: jpegPhoto

Sign in with:
  ada / ada-secret        Ada Lovelace, an administrator, with a picture
  alan / alan-secret      Alan Turing, a user
  grace / grace-secret    Grace Hopper, only with base DN $SUFFIX: the search
                          finds her through the referral to the partners directory
  edsger / edsger-secret  refused: the search filter does not let him in

With base DN $SUFFIX, "Save and Test LDAP Server Settings" stops at the
referral ("Search result reference received, and referral following is off"),
as Jellyfin's plugin does: that test follows no referral, sign-in does.
The root CA path must be readable by the Ferrofin server. Watch binds with 'directory.sh logs'.
EOF
}

up() {
  while (($#)); do
    case $1 in
    --host) HOST=$2; shift 2 ;;
    --bind) BIND=$2; shift 2 ;;
    --dir) DIR=$2; shift 2 ;;
    *) die "unknown option $1" ;;
    esac
  done
  command -v docker >/dev/null || die "needs docker"
  command -v openssl >/dev/null || die "needs openssl"
  docker image inspect "$IMAGE" >/dev/null 2>&1 ||
    printf 'FROM alpine:3.22\nRUN apk add --no-cache openldap openldap-back-mdb openldap-clients\n' |
    docker build -q -t "$IMAGE" - >/dev/null
  mkdir -p "$DIR/main" "$DIR/partners"
  DIR=$(cd "$DIR" && pwd)
  certificates "$DIR/main" "$HOST"
  cp "$DIR/main/"{ca.pem,server.pem,server.key} "$DIR/partners/"
  # slapd runs as root in its container, which reads the key whatever its mode.
  chmod 600 "$DIR"/main/ca.key "$DIR"/*/server.key
  slapd_conf | tee "$DIR/main/slapd.conf" >"$DIR/partners/slapd.conf"
  main_seed >"$DIR/main/seed.ldif"
  partners_seed >"$DIR/partners/seed.ldif"
  echo "$HOST" >"$DIR/host"
  start "$MAIN" "$DIR/main" "$LDAP_PORT" "$LDAPS_PORT"
  start "$PARTNERS" "$DIR/partners" "$PARTNERS_LDAP_PORT" "$PARTNERS_LDAPS_PORT"
  settings
}

modify() {
  docker exec -i "$MAIN" ldapmodify -x -H ldap://localhost -D "$ADMIN_DN" -w "$ADMIN_PASSWORD"
}

case ${1:-} in
up) shift; up "$@" ;;
settings)
  if [[ ${2:-} == --dir ]]; then DIR=${3:?--dir needs a directory}; fi
  settings
  ;;
logs) docker logs -f "$MAIN" ;;
down) docker rm -f "$MAIN" "$PARTNERS" >/dev/null 2>&1 || true ;;
modify) modify ;;
picture)
  (($# == 3)) || die "usage: directory.sh picture UID FILE"
  [[ -r $3 ]] || die "cannot read $3"
  photo=$(openssl base64 -A -in "$3")
  printf 'dn: uid=%s,ou=people,%s\nchangetype: modify\nreplace: jpegPhoto\njpegPhoto:: %s\n' \
    "$2" "$SUFFIX" "$photo" | modify
  ;;
*) sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
