  def install
    bin.install "pishoo"
    libexec.install "pishoo-worker"
    libexec.install "pishoo-ssh-session"

    (etc/"pishoo").mkpath
    chmod 0755, etc/"pishoo"
    etc.install "pishoo.conf" => "pishoo/pishoo.conf" unless File.exist? "#{etc}/pishoo/pishoo.conf"
    etc.install "mime.types"  => "pishoo/mime.types"  unless File.exist? "#{etc}/pishoo/mime.types"
  end

  def post_install
    return if system("/usr/bin/dscl", ".", "-read", "/Groups/pishoo", out: File::NULL, err: File::NULL)

    if Process.uid.zero?
      system "/usr/sbin/dseditgroup", "-o", "create", "pishoo"
    else
      opoo "pishoo group was not found; create it with: sudo dseditgroup -o create pishoo"
    end
  end

  def caveats
    <<~EOS
      Configuration files are installed at:
        #{etc}/pishoo/pishoo.conf

      When workers/groups are not configured, pishoo loads services for users in the pishoo group.
      If the pishoo group was not created automatically, run:
        sudo dseditgroup -o create pishoo
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
