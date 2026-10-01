# frozen_string_literal: true

require "test_helper"
require "lz4-ruby"
require "open3"
require "rbconfig/sizeof"

class RawBlockTest < Minitest::Test
  def test_compress_block_is_readable_by_the_c_decoder
    inputs.each do |input|
      compressed = Lz4Flex.compress_block(input)

      assert_equal(Encoding::BINARY, compressed.encoding)
      decompressed, size = LZ4::Raw.decompress(compressed, input.bytesize)
      assert_equal(input.b, decompressed)
      assert_equal(input.bytesize, size)
    end
  end

  def test_decompress_block_reads_the_c_encoder_output
    inputs.each do |input|
      compressed, = LZ4::Raw.compress(input)
      decompressed = Lz4Flex.decompress_block(compressed, input.bytesize)

      assert_equal(input.b, decompressed)
      assert_equal(Encoding::BINARY, decompressed.encoding)
    end
  end

  def test_decompress_block_returns_the_actual_length_when_capacity_is_larger
    assert_equal("hello".b, Lz4Flex.decompress_block("\x50hello".b, 128))
  end

  def test_decompress_block_rejects_output_larger_than_capacity
    assert_raises(Lz4Flex::DecodeError) { Lz4Flex.decompress_block("\x50hello".b, 4) }
  end

  def test_decompress_block_accepts_an_empty_block_with_zero_capacity
    assert_equal("".b, Lz4Flex.decompress_block("\x00".b, 0))
  end

  def test_decompress_block_rejects_truncated_literals
    assert_raises(Lz4Flex::DecodeError) { Lz4Flex.decompress_block("\x50hell".b, 5) }
  end

  def test_decompress_block_rejects_a_match_before_the_output_start
    assert_raises(Lz4Flex::DecodeError) { Lz4Flex.decompress_block("\x00\x01\x00".b, 16) }
  end

  def test_decompress_block_rejects_an_invalid_offset_with_spare_capacity
    compressed = [0x00, 0x01, 0x00, 0xf0, 0x00].pack("C*") + "a" * 15

    [19, 64].each do |capacity|
      assert_raises(Lz4Flex::DecodeError) { Lz4Flex.decompress_block(compressed, capacity) }
    end
  end

  def test_decompress_block_rejects_a_negative_capacity
    assert_raises(RangeError) { Lz4Flex.decompress_block("\x00".b, -1) }
  end

  def test_decompress_block_rejects_capacity_outside_the_address_space
    assert_raises(RangeError) { Lz4Flex.decompress_block("\x00".b, 2**128) }
  end

  def test_decompress_block_rejects_capacity_above_the_maximum_string_size
    max_string_size = 2**(RbConfig::SIZEOF.fetch("long") * 8 - 1) - 1

    assert_raises(RangeError) { Lz4Flex.decompress_block("\x00".b, max_string_size + 1) }
  end

  def test_decompress_block_unlocks_input_when_output_allocation_fails
    skip("LONG_MAX must be an impossible allocation") if RbConfig::SIZEOF.fetch("long") < 8

    compressed = +"\x00".b
    max_string_size = 2**(RbConfig::SIZEOF.fetch("long") * 8 - 1) - 1

    assert_raises(NoMemoryError) { Lz4Flex.decompress_block(compressed, max_string_size) }
    compressed.replace("\x50hello".b)
    assert_equal("hello".b, Lz4Flex.decompress_block(compressed, 5))
  end

  def test_block_methods_require_string_input
    assert_raises(TypeError) { Lz4Flex.compress_block(nil) }
    assert_raises(TypeError) { Lz4Flex.decompress_block(nil, 0) }
  end

  def test_decompress_block_unlocks_mutable_input_after_an_error
    compressed = +"\x50hell".b

    assert_raises(Lz4Flex::DecodeError) { Lz4Flex.decompress_block(compressed, 16_384) }
    compressed << "o"
    assert_equal("hello".b, Lz4Flex.decompress_block(compressed, 16_384))
  end

  def test_block_methods_preserve_bytes_in_an_unsupported_text_encoding
    input = "こんにちは".encode(Encoding::UTF_16LE)
    compressed = Lz4Flex.compress_block(input)

    assert_equal(input.b, Lz4Flex.decompress_block(compressed, input.bytesize))
  end

  def test_concurrent_calls_return_independent_binary_strings
    threads = 4.times.map do |index|
      Thread.new do
        input = ("thread-#{index}" * 4096).b
        20.times.map do
          compressed = Lz4Flex.compress_block(input)
          Lz4Flex.decompress_block(compressed, input.bytesize) == input
        end
      end
    end

    assert_equal(Array.new(4) { Array.new(20, true) }, threads.map(&:value))
  end

  def test_frozen_and_mutable_strings_survive_gc_stress_and_compaction
    script = <<~RUBY
      GC.stress = true
      GC.auto_compact = true
      [false, true].each do |frozen|
        ["small", "large" * 4096].each do |input|
          input.freeze if frozen
          compressed = Lz4Flex.compress_block(input)
          compressed.freeze if frozen
          GC.compact
          decoded = Lz4Flex.decompress_block(compressed, input.bytesize)
          abort("decoded bytes differ") unless decoded == input.b
          abort("decoded encoding differs") unless decoded.encoding == Encoding::BINARY
          input << "unlocked" unless frozen
          compressed << "unlocked" unless frozen
        end
      end
      puts "four round trips passed"
    RUBY
    stdout, stderr, status = Open3.capture3(RbConfig.ruby, "-Ilib", "-rlz4_flex", "-e", script)

    assert_predicate(status, :success?, stderr)
    assert_equal("four round trips passed\n", stdout)
  end

  private

  def inputs
    ["".b, "\x00\xff".b, "hello".b, "a".b * 65_536, Random.new(42).bytes(65_536)]
  end
end
