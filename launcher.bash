!/bin/bash
export SHELL=/bin/bash
export APP_SETTING="value"
export OPERATOR_DATABASE_URL='postgresql://operator:YOUR_PASSWORD_HERE@127.0.0.1:5432/operator?sslmode=disable'

exec /usr/sbin/operator "$@"
