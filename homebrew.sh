#!/usr/bin/env bash

# 检查必需参数
if [ $# -lt 1 ]; then
    echo "用法: $0 <版本号> [cargo附加参数...]"
    echo "例如: $0 0.2.3 -F sshd,socks"
    exit 1
fi

BINNAME=pishoo
FORMULA=pishoo
VERSION=$1
# 移除版本号，剩余的传给cargo
shift 1
CARGO_ARGS="$@"

DEFAULT_CONFIG=$(cat<<EOF
pishoo {
    server {
        listen all 6080;
        server_name your_domain;

        resolver udp your_dns_server;

        ssl_certificate etc/pishoo/ssl/your_domain.crt;
        ssl_certificate_key etc/pishoo/ssl/your_domain.key;

        # example
        location / {
            proxy_pass       http://127.0.0.1:5500;
        }

        # example
        location /v1 {
            proxy_pass       http://127.0.0.1:8080;
        }
    }
}
EOF)

ARM_TARGET=aarch64-apple-darwin
ARM_WORKDIR=/tmp/pishoo_${VERSION}_arm64
ARM_ARCHIVE=pishoo_${VERSION}_arm64.tar.gz
ARM_ARCHIVE_PATH=$ARM_WORKDIR/$ARM_ARCHIVE
ARM_ARCHIVES_DIR=$ARM_WORKDIR/archives

echo "正在构建 ARM64 (Apple Silicon) 版本..."
cargo build --release --bin $BINNAME --target $ARM_TARGET $CARGO_ARGS

echo "构建完成，正在打包... 缓存路径: $ARM_WORKDIR"
mkdir -p $ARM_ARCHIVES_DIR
cp target/$ARM_TARGET/release/$BINNAME $ARM_ARCHIVES_DIR
cat>$ARM_ARCHIVES_DIR/pishoo.conf<<EOF
$DEFAULT_CONFIG
EOF
tar -czvf $ARM_ARCHIVE_PATH -C $ARM_ARCHIVES_DIR .

ARM_ARCHIVE_SHA256=$(shasum -a 256 $ARM_ARCHIVE_PATH | cut -d ' ' -f 1)
echo "ARM64 打包完成，SHA256: $ARM_ARCHIVE_SHA256 $ARM_ARCHIVE_PATH"

AMD_TARGET=x86_64-apple-darwin
AMD_WORKDIR=/tmp/pishoo_${VERSION}_amd64
AMD_ARCHIVE=pishoo_${VERSION}_amd64.tar.gz
AMD_ARCHIVE_PATH=$AMD_WORKDIR/$AMD_ARCHIVE
AMD_ARCHIVES_DIR=$AMD_WORKDIR/archives

echo "正在构建 AMD64 (Intel) 版本..."
cargo build --release --bin $BINNAME --target $AMD_TARGET $CARGO_ARGS

echo "构建完成，正在打包... 缓存路径: $AMD_WORKDIR"
mkdir -p $AMD_ARCHIVES_DIR
cp target/$AMD_TARGET/release/$BINNAME $AMD_ARCHIVES_DIR
cat>$AMD_ARCHIVES_DIR/pishoo.conf<<EOF
$DEFAULT_CONFIG
EOF
tar -czvf $AMD_ARCHIVE_PATH -C $AMD_ARCHIVES_DIR .

AMD_ARCHIVE_SHA256=$(shasum -a 256 $AMD_ARCHIVE_PATH | cut -d ' ' -f 1)
echo "AMD64 打包完成，SHA256: $AMD_ARCHIVE_SHA256 $AMD_ARCHIVE_PATH"

echo "构建归档位于:"
echo "ARM64: $ARM_ARCHIVE_PATH"
echo "AMD64: $AMD_ARCHIVE_PATH"

echo "上传文件到服务器:"
rsync --rsync-path="sudo rsync" $ARM_ARCHIVE_PATH $AMD_ARCHIVE_PATH ubuntu@download.genmeta.net:/data/wwwroot/homebrew/

# 确保homebrew-genmeta目录存在
if [ ! -d "../homebrew-genmeta" ]; then
    echo "错误: ../homebrew-genmeta 目录不存在"
    echo "请先 git clone git@github.com:genmeta/homebrew-genmeta.git"
    exit 1
fi

echo "生成 Homebrew formula..."
cat>../homebrew-genmeta/pishoo.rb<<EOF
class Pishoo < Formula
  desc "Pishoo (\"Prosperity Guardian Beast\") is a powerful proxy server optimized for HTTP/3 and end-to-end encrypted communication. Built for privacy and security scenarios, it seamlessly functions as both a forward proxy for client privacy and a reverse proxy to streamline traffic between edge networks and backend services. Its architecture is designed to safeguard your data from infringement and ensures that it can be accessed and utilized securely."
  version "${VERSION}"

  on_arm do
    url "https://download.genmeta.net/homebrew/$ARM_ARCHIVE"
    sha256 "$ARM_ARCHIVE_SHA256"
  end
  
  on_intel do
    url "https://download.genmeta.net/homebrew/$AMD_ARCHIVE"
    sha256 "$AMD_ARCHIVE_SHA256"
  end

  def install
    bin.install "pishoo"
    
    (etc/"pishoo").mkpath
    
    etc.install "pishoo.conf" => "pishoo/pishoo.conf" unless File.exist? "#{etc}/pishoo/pishoo.conf"
    
    (etc/"pishoo/ssl").mkpath
  end

  def post_install
    chmod 0755, etc/"pishoo"
    chmod 0755, etc/"pishoo/ssl" 
    chmod 0644, etc/"pishoo/pishoo.conf" if File.exist? "#{etc}/pishoo/pishoo.conf"
  end

  def caveats
    <<~EOS
      Configuration files are installed at:
        #{etc}/pishoo/pishoo.conf
      
      The SSL certificates should be placed in:
        #{etc}/pishoo/ssl/
    EOS
  end

  service do
    run [opt_bin/"pishoo", "-c", etc/"pishoo/pishoo.conf"]
    keep_alive true
    log_path var/"log/pishoo.log"
    error_log_path var/"log/pishoo.error.log"
    working_dir HOMEBREW_PREFIX
  end

  test do
    system "#{bin}/pishoo", "-V"
  end
end
EOF

echo "提交变更到 homebrew-genmeta 仓库..."
cd ../homebrew-genmeta/
git add pishoo.rb
git commit -S -m "feat: release pishoo v${VERSION}"
echo "打包完成！请检查并推送仓库更改。"

echo "清理临时文件..."
rm -r $ARM_WORKDIR $AMD_WORKDIR