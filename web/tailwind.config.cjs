module.exports = {
  content: ['./index.html', './src/**/*.{ts,tsx}'],
  theme: { extend: {
    colors: { ink:'#0c1425', primary:'#2563eb', teal:'#06b6a3' },
    boxShadow: { soft:'0 12px 35px rgba(14,36,73,.06)' },
    borderRadius: { panel:'1.2rem' }
  } },
  plugins: []
}
