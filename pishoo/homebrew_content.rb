  def install
    bin.install "pishoo"
    libexec.install "pishoo-worker"
    libexec.install "pishoo-ssh-session"

    (etc/"pishoo").mkpath
    chmod 0755, etc/"pishoo"
    etc.install "pishoo.conf" => "pishoo/pishoo.conf" unless File.exist? "#{etc}/pishoo/pishoo.conf"
    etc.install "mime.types"  => "pishoo/mime.types"  unless File.exist? "#{etc}/pishoo/mime.types"

    (etc/"pishoo/ssl").mkpath
    chmod 0700, etc/"pishoo/ssl"

    (etc/"pishoo/acl").mkpath
    chmod 0700, etc/"pishoo/acl"
    begin
      touch etc/"pishoo/acl/rules.db"
    rescue
      opoo "Failed to initial access rule database at #{etc}/pishoo/acl/rules.db. If this is not the first installation, this warning can be ignored."
    end
  end

  def caveats
    <<~EOS
      Configuration files are installed at:
        #{etc}/pishoo/pishoo.conf

      The SSL certificates should be placed in:
        #{etc}/pishoo/ssl/
    
      For the firest install, empty access rule database file created at:
        #{etc}/pishoo/acl/rules.db
      You can install `access` to configure firewall rules
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
