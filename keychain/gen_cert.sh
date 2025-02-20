#!/bin/bash

if [ -z "$1" ]; then
  echo "Usage: $0 <domain>"
  exit 1
fi

DOMAIN=$1

# 生成服务器私钥
openssl ecparam -name secp384r1 -genkey -noout -out "${DOMAIN}.key"

# 创建证书签名请求 (CSR)
openssl req -new -key "${DOMAIN}.key" -out "${DOMAIN}.csr" -subj "/CN=${DOMAIN}"

# 生成 openssl 配置文件
cat <<EOT > openssl.cnf
[v3_req]
basicConstraints = CA:FALSE
keyUsage = nonRepudiation, digitalSignature, keyEncipherment
subjectAltName = @alt_names

[alt_names]
DNS.1 = ${DOMAIN}
EOT

# 使用根证书签名服务器证书
openssl x509 -req \
  -extfile openssl.cnf -extensions v3_req \
  -in "${DOMAIN}.csr" \
  -CA root.crt -CAkey root.key -CAcreateserial \
  -out "${DOMAIN}.crt" -days 365 -sha384

# 清理临时文件
rm -f openssl.cnf "${DOMAIN}.csr" root.srl

echo "Server private key and certificate generated successfully:"
echo "Private key: ${DOMAIN}.key"
echo "Certificate: ${DOMAIN}.crt"